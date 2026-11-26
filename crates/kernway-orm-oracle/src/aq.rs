//! Oracle Advanced Queuing: enqueue and dequeue messages with RAW, JSON or
//! object-type payloads and their properties (correlation, priority, delay,
//! expiration), dequeue waiting / browsing / by correlation or consumer.
//! Each operation is one round trip through `DBMS_AQ` (the same server
//! procedures the native AQ messages run), with payloads bound like any
//! other value — objects pickled with their type.

use kernway_orm_core::error::OrmError;

use crate::exec::{Bind, OutType};
use crate::value::Value;
use crate::Conn;

/// What a queue carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AqPayload {
    /// `RAW` payloads (up to 32 KB): [`Value::Raw`].
    Raw,
    /// `JSON` payloads (Oracle 21c+): text, [`Value::Str`].
    Json,
    /// An object type (`SCHEMA.NAME`): [`Value::Object`].
    Object(String),
}

/// A message to enqueue, or one dequeued.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AqMessage {
    /// The payload ([`Value::Raw`], JSON text, or [`Value::Object`]).
    pub payload: Value,
    /// Correlation id (for selective dequeue).
    pub correlation: Option<String>,
    /// Priority (lower dequeues first on a priority-ordered queue).
    pub priority: i32,
    /// Seconds before the message can be dequeued.
    pub delay: i32,
    /// Seconds the message stays dequeuable (-1 = forever).
    pub expiration: i32,
    /// Message id (set by enqueue / dequeue).
    pub msg_id: Vec<u8>,
    /// Dequeue attempts so far (dequeued messages).
    pub attempts: i32,
}

impl AqMessage {
    /// A message with `payload` and default properties.
    pub fn new(payload: Value) -> Self {
        Self { payload, expiration: -1, ..Default::default() }
    }
}

/// How to dequeue.
#[derive(Debug, Clone, Default)]
pub struct AqDequeue {
    /// Seconds to wait for a message (`None` = do not wait).
    pub wait: Option<u32>,
    /// Only a message with this correlation id.
    pub correlation: Option<String>,
    /// Consumer (multi-consumer queues).
    pub consumer: Option<String>,
    /// Read without removing.
    pub browse: bool,
    /// Visible at once rather than with the transaction's commit.
    pub immediate: bool,
}

const NO_MESSAGE: i64 = 25228; // ORA-25228: timeout or end of fetch

fn declare(payload: &AqPayload) -> String {
    match payload {
        AqPayload::Raw => "RAW(32767)".into(),
        AqPayload::Json => "JSON".into(),
        AqPayload::Object(t) => t.clone(),
    }
}

impl Conn {
    /// Enqueue `msg` on `queue`; returns its message id. With `immediate`
    /// the message is visible at once, else when the transaction commits.
    pub async fn aq_enqueue(&mut self, queue: &str, payload: &AqPayload, msg: &AqMessage, immediate: bool) -> Result<Vec<u8>, OrmError> {
        let (bind_payload, expr) = match (payload, &msg.payload) {
            (AqPayload::Raw, v @ Value::Raw(_)) => (v.clone(), ":p".to_string()),
            (AqPayload::Json, Value::Str(s)) => (Value::Str(s.clone()), "JSON(:p)".to_string()),
            (AqPayload::Object(t), Value::Object { fields, .. }) => (Value::Object { type_name: t.clone(), fields: fields.clone() }, ":p".to_string()),
            (p, v) => return Err(OrmError::TypeConversion(format!("a {p:?} queue cannot take {v:?}"))),
        };
        let sql = format!(
            "DECLARE eo DBMS_AQ.ENQUEUE_OPTIONS_T; mp DBMS_AQ.MESSAGE_PROPERTIES_T; id RAW(16);
             BEGIN
               IF :imm = 1 THEN eo.visibility := DBMS_AQ.IMMEDIATE; END IF;
               mp.correlation := :corr; mp.priority := :prio; mp.delay := :dly; mp.expiration := :expi;
               DBMS_AQ.ENQUEUE(queue_name => :q, enqueue_options => eo, message_properties => mp, payload => {expr}, msgid => id);
               :id := id;
             END;"
        );
        let r = self
            .execute(
                &sql,
                &[
                    Bind::In(Value::Number(u8::from(immediate).to_string())),
                    Bind::In(msg.correlation.clone().map_or(Value::Null, Value::Str)),
                    Bind::In(Value::Number(msg.priority.to_string())),
                    Bind::In(Value::Number(msg.delay.to_string())),
                    Bind::In(Value::Number(msg.expiration.to_string())),
                    Bind::In(Value::Str(queue.to_string())),
                    Bind::In(bind_payload),
                    Bind::Out(OutType::Raw(16)),
                ],
            )
            .await?;
        match &r.out[7] {
            Value::Raw(id) => Ok(id.clone()),
            other => Err(OrmError::Query(format!("enqueue returned no message id: {other:?}"))),
        }
    }

    /// Dequeue the next message from `queue` (`None` when there is none
    /// within the wait).
    pub async fn aq_dequeue(&mut self, queue: &str, payload: &AqPayload, opts: &AqDequeue) -> Result<Option<AqMessage>, OrmError> {
        let (out_expr, out_type) = match payload {
            AqPayload::Raw => ("p", OutType::Raw(32767)),
            AqPayload::Json => ("JSON_SERIALIZE(p RETURNING CLOB)", OutType::Clob),
            AqPayload::Object(t) => ("p", OutType::Object(t.clone())),
        };
        let sql = format!(
            "DECLARE dq DBMS_AQ.DEQUEUE_OPTIONS_T; mp DBMS_AQ.MESSAGE_PROPERTIES_T; id RAW(16); p {decl};
             BEGIN
               dq.wait := NVL(:wt, DBMS_AQ.NO_WAIT);
               dq.correlation := :corr; dq.consumer_name := :cons;
               IF :brw = 1 THEN dq.dequeue_mode := DBMS_AQ.BROWSE; END IF;
               IF :imm = 1 THEN dq.visibility := DBMS_AQ.IMMEDIATE; END IF;
               dq.navigation := DBMS_AQ.FIRST_MESSAGE;
               BEGIN
                 DBMS_AQ.DEQUEUE(queue_name => :q, dequeue_options => dq, message_properties => mp, payload => p, msgid => id);
               EXCEPTION WHEN OTHERS THEN
                 IF SQLCODE = -{NO_MESSAGE} THEN :found := 0; RETURN; END IF;
                 RAISE;
               END;
               :found := 1; :pl := {out_expr}; :id := id; :corr_o := mp.correlation; :prio := mp.priority;
               :att := mp.attempts; :expi := mp.expiration;
             END;",
            decl = declare(payload)
        );
        let opt_str = |o: &Option<String>| o.clone().map_or(Value::Null, Value::Str);
        let r = self
            .execute(
                &sql,
                &[
                    Bind::In(opts.wait.map_or(Value::Null, |w| Value::Number(w.to_string()))),
                    Bind::In(opt_str(&opts.correlation)),
                    Bind::In(opt_str(&opts.consumer)),
                    Bind::In(Value::Number(u8::from(opts.browse).to_string())),
                    Bind::In(Value::Number(u8::from(opts.immediate).to_string())),
                    Bind::In(Value::Str(queue.to_string())),
                    Bind::Out(OutType::Number),
                    Bind::Out(out_type),
                    Bind::Out(OutType::Raw(16)),
                    Bind::Out(OutType::Str(128)),
                    Bind::Out(OutType::Number),
                    Bind::Out(OutType::Number),
                    Bind::Out(OutType::Number),
                ],
            )
            .await?;
        let o = &r.out;
        if o[6].as_i64() != Some(1) {
            return Ok(None);
        }
        let num = |v: &Value| v.as_i64().unwrap_or(0) as i32;
        Ok(Some(AqMessage {
            payload: o[7].clone(),
            msg_id: match &o[8] {
                Value::Raw(b) => b.clone(),
                _ => Vec::new(),
            },
            correlation: o[9].to_text(),
            priority: num(&o[10]),
            attempts: num(&o[11]),
            expiration: num(&o[12]),
            delay: 0,
        }))
    }
}
