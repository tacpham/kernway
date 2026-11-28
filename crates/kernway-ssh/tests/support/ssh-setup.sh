#!/usr/bin/env bash
# An OpenSSH server for the client's live tests, with fresh keys:
#
#   127.0.0.1:2222   sshd: user keys ed25519 / ecdsa / rsa (and a passphrase-protected
#                    one), password login where the setup can offer it, the sftp subsystem
#   127.0.0.1:2223   the same server with RekeyLimit 64K, so it rekeys mid-transfer
#
# SSH_DOCKER=1: the linuxserver/openssh-server image (password login works: the
# container's sshd runs as root). Otherwise sshd runs natively as the current user —
# OpenSSH on Linux and macOS — with public-key login only (a non-root sshd cannot
# check passwords).
#
#   crates/kernway-ssh/tests/support/ssh-setup.sh [dir]
#
# Prints the test environment (KERNWAY_SSH_*) as KEY=VALUE lines.
set -euo pipefail
DIR="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/ssh}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
cd "$DIR"

# Fresh user keys (and the host keys for a native server).
mkdir -p keys
for k in id_ed25519 id_ecdsa id_ecdsa384 id_rsa id_rsa_pkcs8 id_rsa_pem id_ed25519_enc id_ecdsa_enc id_rsa_enc id_ed25519_gcm id_agent host_ed25519 host_ecdsa host_rsa; do rm -f "keys/$k" "keys/$k.pub"; done
ssh-keygen -q -t ed25519 -N '' -C kw-ed25519 -f keys/id_ed25519
ssh-keygen -q -t ecdsa -b 256 -N '' -C kw-ecdsa -f keys/id_ecdsa
ssh-keygen -q -t ecdsa -b 384 -N '' -C kw-ecdsa384 -f keys/id_ecdsa384
ssh-keygen -q -t rsa -b 3072 -N '' -C kw-rsa -f keys/id_rsa
# Passphrase-protected, as ssh-keygen writes them (bcrypt_pbkdf + aes256-ctr), and
# one with the AES-GCM cipher; a key only an ssh-agent holds.
ssh-keygen -q -t ed25519 -N 'a passphrase' -C kw-enc -f keys/id_ed25519_enc
ssh-keygen -q -t ecdsa -b 256 -N 'mật khẩu ✓' -C kw-ecdsa-enc -f keys/id_ecdsa_enc
ssh-keygen -q -t rsa -b 3072 -N 'a passphrase' -C kw-rsa-enc -f keys/id_rsa_enc
ssh-keygen -q -t ed25519 -N 'a passphrase' -Z aes256-gcm@openssh.com -C kw-gcm -f keys/id_ed25519_gcm
ssh-keygen -q -t ed25519 -N '' -C kw-agent -f keys/id_agent
# The same RSA key as PKCS#8 and PKCS#1 PEM, to prove those formats too.
cp keys/id_rsa keys/id_rsa_pkcs8 && ssh-keygen -q -p -N '' -m PKCS8 -f keys/id_rsa_pkcs8 >/dev/null
cp keys/id_rsa keys/id_rsa_pem && ssh-keygen -q -p -N '' -m PEM -f keys/id_rsa_pem >/dev/null
cat keys/id_ed25519.pub keys/id_ecdsa.pub keys/id_ecdsa384.pub keys/id_rsa.pub keys/id_ed25519_enc.pub \
    keys/id_ecdsa_enc.pub keys/id_rsa_enc.pub keys/id_ed25519_gcm.pub keys/id_agent.pub > authorized_keys
chmod 600 keys/* authorized_keys

PASSWORD="pässwörd 123"
if [ -n "${SSH_DOCKER:-}" ]; then
  IMAGE="${SSH_IMAGE:-lscr.io/linuxserver/openssh-server:latest}"
  rm -rf pub && mkdir -p pub && cp authorized_keys pub/all.pub
  docker rm -f kernway-sshd >/dev/null 2>&1 || true
  docker run -d --name kernway-sshd -p 2222:2222 -p 2223:2223 -e PUID=1000 -e PGID=1000 -e USER_NAME=kw \
    -e PASSWORD_ACCESS=true -e USER_PASSWORD="$PASSWORD" -e PUBLIC_KEY_DIR=/pubkeys -v "$DIR/pub":/pubkeys:ro "$IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    if docker exec kernway-sshd sh -c 'test -s /config/.ssh/authorized_keys && pgrep -x sshd.pam >/dev/null || pgrep sshd >/dev/null' 2>/dev/null \
      && ssh-keyscan -p 2222 127.0.0.1 >/dev/null 2>&1; then break; fi
    sleep 1
  done
  # The tests fail logins on purpose: no penalty box for that (OpenSSH 9.8+), then
  # restart sshd with it.
  docker exec kernway-sshd sh -c 'grep -q PerSourcePenalties /config/sshd/sshd_config || echo "PerSourcePenalties no" >> /config/sshd/sshd_config'
  # Port forwarding (the image turns it off) for the -L / -D / -R tests.
  docker exec kernway-sshd sh -c 'sed -i "s/^AllowTcpForwarding no/AllowTcpForwarding yes/" /config/sshd/sshd_config'
  docker restart kernway-sshd >/dev/null
  for _ in $(seq 1 60); do ssh-keyscan -p 2222 127.0.0.1 >/dev/null 2>&1 && break; sleep 1; done
  # The rekeying twin, in the same container.
  docker exec -d kernway-sshd sh -c 'exec $(command -v sshd.pam || command -v sshd) -D -e -p 2223 -f /config/sshd/sshd_config -o RekeyLimit=64K'
  USER_NAME=kw
  SERVER="$(docker exec kernway-sshd sh -c 'ssh -V 2>&1' | tr -d '\r') (Docker, $IMAGE)"
else
  SSHD="${SSHD_BIN:-$(command -v sshd || echo /usr/sbin/sshd)}"
  for t in ed25519 ecdsa rsa; do ssh-keygen -q -t "$t" -N '' -f "keys/host_$t"; done
  stop() { if [ -f "$DIR/$1.pid" ]; then kill "$(cat "$DIR/$1.pid")" 2>/dev/null || true; rm -f "$DIR/$1.pid"; sleep 1; fi; }
  stop sshd; stop sshd-rekey
  SFTP_SERVER="$(for p in /usr/lib/openssh/sftp-server /usr/libexec/sftp-server /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server; do [ -x "$p" ] && echo "$p" && break; done)"
  cat > sshd_config <<EOF
ListenAddress 127.0.0.1
HostKey $DIR/keys/host_ed25519
HostKey $DIR/keys/host_ecdsa
HostKey $DIR/keys/host_rsa
AuthorizedKeysFile $DIR/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
StrictModes no
UsePAM no
AllowTcpForwarding yes
PidFile none
Subsystem sftp ${SFTP_SERVER:-internal-sftp}
EOF
  # The tests fail logins on purpose: no penalty box for that (OpenSSH 9.8+ only).
  if "$SSHD" -t -f "$DIR/sshd_config" -o PerSourcePenalties=no 2>/dev/null; then echo "PerSourcePenalties no" >> sshd_config; fi
  "$SSHD" -D -e -p 2222 -f "$DIR/sshd_config" > "$DIR/sshd.out" 2>&1 &
  echo $! > "$DIR/sshd.pid"
  "$SSHD" -D -e -p 2223 -f "$DIR/sshd_config" -o RekeyLimit=64K > "$DIR/sshd-rekey.out" 2>&1 &
  echo $! > "$DIR/sshd-rekey.pid"
  USER_NAME="$(id -un)"
  PASSWORD=""
  SERVER="$("$SSHD" -V 2>&1 | head -1 | tr -d '\r') (native)"
  [ "$SERVER" = " (native)" ] && SERVER="$(ssh -V 2>&1 | tr -d '\r') (native sshd)"
fi

for port in 2222 2223; do
  for _ in $(seq 1 60); do
    if ssh-keyscan -p "$port" 127.0.0.1 >/dev/null 2>&1; then break; fi
    sleep 0.5
  done
done
# known_hosts as `ssh` itself would write it, from the server's own answer.
ssh-keyscan -p 2222 127.0.0.1 2>/dev/null > known_hosts
ssh-keyscan -p 2223 127.0.0.1 2>/dev/null >> known_hosts
[ -s known_hosts ] || { echo "sshd did not start" >&2; cat "$DIR"/sshd*.out >&2 2>/dev/null || true; docker logs kernway-sshd 2>&1 | tail -20 >&2 || true; exit 1; }

# An ssh-agent holding id_agent (its key file is then moved away: only the agent can sign).
# A Unix socket path is limited to ~104 bytes: keep it short, out of $DIR.
AGENT_SOCK="/tmp/kw-ssh-agent-$(id -u).sock"
if [ -f "$DIR/agent.pid" ]; then kill "$(cat "$DIR/agent.pid")" 2>/dev/null || true; fi
rm -f "$AGENT_SOCK"
eval "$(ssh-agent -a "$AGENT_SOCK" -s)" >/dev/null
echo "$SSH_AGENT_PID" > "$DIR/agent.pid"
SSH_AUTH_SOCK="$AGENT_SOCK" ssh-add -q keys/id_agent
mv keys/id_agent keys/id_agent.held-by-agent

echo "KERNWAY_SSH_HOST=127.0.0.1"
echo "KERNWAY_SSH_PORT=2222"
echo "KERNWAY_SSH_REKEY_PORT=2223"
echo "KERNWAY_SSH_USER=$USER_NAME"
echo "KERNWAY_SSH_PASSWORD=$PASSWORD"
echo "KERNWAY_SSH_DIR=$DIR"
echo "KERNWAY_SSH_AGENT_SOCK=$AGENT_SOCK"
echo "KERNWAY_SSH_SERVER=$SERVER"
