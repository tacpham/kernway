//! The unified test format's entity map.

use kernway_orm_mongodb::{Bson, ChangeStream, Client, Collection, CommandEvent, Cursor, Database, Document, GridFsBucket, Session};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A client entity and the command events it observed.
pub struct ClientEntity {
    pub client: Client,
    pub events: Rc<RefCell<Vec<CommandEvent>>>,
    /// `observeEvents` (command events only; `None`: none observed).
    pub observe: Vec<String>,
    /// `ignoreCommandMonitoringEvents`.
    pub ignore: Vec<String>,
    /// `observeSensitiveCommands`.
    pub observe_sensitive: bool,
}

pub enum Entity {
    Client(Rc<ClientEntity>),
    Database(Box<Database>),
    Collection(Box<Collection<Document>>),
    /// The session and its lsid, recorded once known (kept after it ends).
    Session(Rc<Session>, RefCell<Option<Document>>),
    Bucket(Box<GridFsBucket>),
    Cursor(Box<Cursor<Document>>),
    ChangeStream(Box<ChangeStream>),
    Value(Bson),
}

#[derive(Default)]
pub struct Entities {
    map: HashMap<String, Entity>,
}

impl Entities {
    pub fn insert(&mut self, id: &str, e: Entity) -> Result<(), String> {
        if self.map.contains_key(id) {
            return Err(format!("entity {id} already defined"));
        }
        self.map.insert(id.to_string(), e);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Entity> {
        self.map.get(id)
    }

    pub fn remove(&mut self, id: &str) -> Option<Entity> {
        self.map.remove(id)
    }

    pub fn client(&self, id: &str) -> Result<Rc<ClientEntity>, String> {
        match self.map.get(id) {
            Some(Entity::Client(c)) => Ok(c.clone()),
            _ => Err(format!("no client entity {id}")),
        }
    }

    pub fn database(&self, id: &str) -> Result<Database, String> {
        match self.map.get(id) {
            Some(Entity::Database(d)) => Ok((**d).clone()),
            _ => Err(format!("no database entity {id}")),
        }
    }

    pub fn collection(&self, id: &str) -> Result<Collection<Document>, String> {
        match self.map.get(id) {
            Some(Entity::Collection(c)) => Ok((**c).clone()),
            _ => Err(format!("no collection entity {id}")),
        }
    }

    pub fn session(&self, id: &str) -> Result<Rc<Session>, String> {
        match self.map.get(id) {
            Some(Entity::Session(s, _)) => Ok(s.clone()),
            _ => Err(format!("no session entity {id}")),
        }
    }

    pub fn session_lsid(&self, id: &str) -> Option<Document> {
        match self.map.get(id) {
            Some(Entity::Session(s, lsid)) => lsid.borrow().clone().or_else(|| s.id()),
            _ => None,
        }
    }

    /// Remember each session's lsid as soon as it has one.
    pub fn record_lsids(&self) {
        for e in self.map.values() {
            if let Entity::Session(s, lsid) = e {
                if lsid.borrow().is_none() {
                    *lsid.borrow_mut() = s.id();
                }
            }
        }
    }

    pub fn value(&self, id: &str) -> Option<Bson> {
        match self.map.get(id) {
            Some(Entity::Value(v)) => Some(v.clone()),
            _ => None,
        }
    }

    pub fn clients(&self) -> Vec<Rc<ClientEntity>> {
        self.map
            .values()
            .filter_map(|e| match e {
                Entity::Client(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every entity id, for closing what a test leaves open.
    pub fn ids(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
}
