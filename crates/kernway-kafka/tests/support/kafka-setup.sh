#!/usr/bin/env bash
# A Kafka broker (KRaft, one node) for the client's tests:
#
#   PLAINTEXT      127.0.0.1:9092
#   SASL_PLAINTEXT 127.0.0.1:9093   PLAIN kw / kw_pw; SCRAM-SHA-256 and
#                                   SCRAM-SHA-512 kw / kw_pw
#   SSL            localhost:9094   server cert for DNS:localhost from ca.pem;
#                                   a client certificate is checked when sent
#
# Docker by default (KAFKA_IMAGE, default apache/kafka:3.9.1), or the
# official release run natively when KAFKA_HOME names an unpacked
# kafka_2.13-<version> (macOS / Windows CI; java on the PATH, called
# directly rather than through the release's .sh / .bat scripts).
#
#   crates/kernway-kafka/tests/support/kafka-setup.sh [dir]
#
# Prints the test environment (KERNWAY_KAFKA_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/kafka}"
IMAGE="${KAFKA_IMAGE:-apache/kafka:3.9.1}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"
WIN=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WIN=1 ;; esac
native_path() { if [ -n "$WIN" ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
MODE=docker
if [ -n "${KAFKA_HOME:-}" ]; then MODE=native; fi

# ---- certificates (PEM; the key as unencrypted PKCS#8) ---------------------------
export MSYS_NO_PATHCONV=1
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=kernway test CA" -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.crt 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in server.key -out server.p8
cat server.crt server.p8 > server.pem
openssl req -newkey rsa:2048 -nodes -subj "/CN=kw-client" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.crt 2>/dev/null
unset MSYS_NO_PATHCONV
chmod 644 ./*.pem ./*.key ./*.crt ./*.p8

# ---- config ------------------------------------------------------------------------
if [ "$MODE" = docker ]; then CONF=/etc/kw; LOGS=/tmp/kraft-logs; else CONF="$(native_path "$DIR")"; LOGS="$(native_path "$DIR/data")"; fi
cat > server.properties <<EOF
process.roles=broker,controller
node.id=1
controller.quorum.voters=1@localhost:29093
listeners=PLAINTEXT://:9092,SASL_PLAINTEXT://:9093,SSL://:9094,CONTROLLER://:29093
advertised.listeners=PLAINTEXT://127.0.0.1:9092,SASL_PLAINTEXT://127.0.0.1:9093,SSL://localhost:9094
listener.security.protocol.map=PLAINTEXT:PLAINTEXT,SASL_PLAINTEXT:SASL_PLAINTEXT,SSL:SSL,CONTROLLER:PLAINTEXT
controller.listener.names=CONTROLLER
inter.broker.listener.name=PLAINTEXT
sasl.enabled.mechanisms=PLAIN,SCRAM-SHA-256,SCRAM-SHA-512
listener.name.sasl_plaintext.plain.sasl.jaas.config=org.apache.kafka.common.security.plain.PlainLoginModule required user_kw="kw_pw";
listener.name.sasl_plaintext.scram-sha-256.sasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required;
listener.name.sasl_plaintext.scram-sha-512.sasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required;
ssl.keystore.type=PEM
ssl.keystore.location=$CONF/server.pem
ssl.truststore.type=PEM
ssl.truststore.location=$CONF/ca.pem
ssl.client.auth=requested
log.dirs=$LOGS
num.partitions=3
offsets.topic.replication.factor=1
offsets.topic.num.partitions=3
transaction.state.log.replication.factor=1
transaction.state.log.min.isr=1
group.initial.rebalance.delay.ms=0
group.min.session.timeout.ms=3000
EOF
SCRAM=(--add-scram 'SCRAM-SHA-256=[name=kw,password=kw_pw]' --add-scram 'SCRAM-SHA-512=[name=kw,password=kw_pw]')

# ---- start ----------------------------------------------------------------------------
if [ "$MODE" = docker ]; then
  docker rm -f kernway-kafka >/dev/null 2>&1 || true
  docker run -d --name kernway-kafka -p 9092:9092 -p 9093:9093 -p 9094:9094 -v "$DIR":/etc/kw:ro --entrypoint bash "$IMAGE" -c \
    "/opt/kafka/bin/kafka-storage.sh format --ignore-formatted -t kw-test-cluster-0000001 -c /etc/kw/server.properties \
       --add-scram 'SCRAM-SHA-256=[name=kw,password=kw_pw]' --add-scram 'SCRAM-SHA-512=[name=kw,password=kw_pw]' >/dev/null \
     && exec /opt/kafka/bin/kafka-server-start.sh /etc/kw/server.properties" >/dev/null
  TOOLS="docker exec -i kernway-kafka /opt/kafka/bin"
  RESTART="docker restart kernway-kafka"
  EXT=".sh"
  VERSION="$(docker exec kernway-kafka ls /opt/kafka/libs | sed -n 's/^kafka_2\.13-\([0-9.]*\)\.jar$/\1/p' | head -1)"
else
  # Java directly (not the .sh / .bat wrappers): cmd.exe splits the SCRAM
  # specs at their = and , — the JVM gets its arguments intact on every OS.
  if [ -f "$DIR/pid" ]; then kill "$(cat "$DIR/pid")" 2>/dev/null || true; sleep 2; fi
  rm -rf "$DIR/data"
  KH="$(native_path "$KAFKA_HOME")"
  mkdir -p "$DIR/logs"
  JOPTS=(-Xmx1G "-Dkafka.logs.dir=$(native_path "$DIR/logs")" "-Dlog4j.configuration=file:$KH/config/log4j.properties" -cp "$KH/libs/*")
  java "${JOPTS[@]}" kafka.tools.StorageTool format -t kw-test-cluster-0000001 -c "$(native_path "$DIR/server.properties")" "${SCRAM[@]}" >/dev/null
  java "${JOPTS[@]}" kafka.Kafka "$(native_path "$DIR/server.properties")" >> "$DIR/server.out" 2>&1 &
  echo $! > "$DIR/pid"
  # The command-line tools the tests use, as scripts calling the JVM.
  mkdir -p "$DIR/bin"
  printf '#!/usr/bin/env bash\nexec java -Xmx512M "-Dlog4j.configuration=file:%s/config/tools-log4j.properties" -cp "%s/libs/*" kafka.tools.ConsoleProducer "$@"\n' "$KH" "$KH" > "$DIR/bin/kafka-console-producer.sh"
  # A restart for the tests: stop the JVM, wait for the port to close, start again.
  cat > "$DIR/restart.sh" <<EOS
#!/usr/bin/env bash
if [ -n "$WIN" ]; then
  pid=\$(netstat -ano | grep -E 'TCP +[^ ]+:9092 +[^ ]+ +LISTENING' | awk '{print \$NF}' | head -1)
  [ -n "\$pid" ] && taskkill //F //PID "\$pid" >/dev/null 2>&1
else
  kill "\$(cat "$DIR/pid")" 2>/dev/null
fi
for _ in \$(seq 1 60); do (exec 3<>/dev/tcp/127.0.0.1/9092) 2>/dev/null || break; sleep 0.5; done
java -Xmx1G "-Dkafka.logs.dir=$(native_path "$DIR/logs")" "-Dlog4j.configuration=file:$KH/config/log4j.properties" -cp "$KH/libs/*" kafka.Kafka "$(native_path "$DIR/server.properties")" >> "$DIR/server.out" 2>&1 &
echo \$! > "$DIR/pid"
EOS
  chmod +x "$DIR/restart.sh" "$DIR/bin/kafka-console-producer.sh"
  RESTART="bash \"$DIR/restart.sh\""
  TOOLS="bash \"$DIR/bin\""
  EXT=".sh"
  VERSION="$(basename "$KAFKA_HOME" | sed 's/^kafka_2\.13-//')"
fi
for _ in $(seq 1 120); do
  if (exec 3<>/dev/tcp/127.0.0.1/9092) 2>/dev/null && (exec 3<>/dev/tcp/127.0.0.1/9094) 2>/dev/null; then break; fi
  sleep 1
done
(exec 3<>/dev/tcp/127.0.0.1/9092) 2>/dev/null || { echo "Kafka did not start; see $DIR" >&2; [ "$MODE" = docker ] && docker logs kernway-kafka 2>&1 | tail -30 >&2; exit 1; }
sleep 2

echo "KERNWAY_KAFKA_URL=kafka://127.0.0.1:9092"
echo "KERNWAY_KAFKA_DIR=$(native_path "$DIR")"
# Runs the Kafka command-line tools: <tools>/kafka-console-producer<ext> …
echo "KERNWAY_KAFKA_TOOLS=$TOOLS"
echo "KERNWAY_KAFKA_TOOL_EXT=$EXT"
echo "KERNWAY_KAFKA_SHELL=$(native_path "$BASH")"
# Stops and starts the broker (a real outage for the reconnect test).
echo "KERNWAY_KAFKA_RESTART=$RESTART"
echo "KERNWAY_KAFKA_VERSION=$VERSION"
