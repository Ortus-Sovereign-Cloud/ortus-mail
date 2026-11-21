#!/usr/bin/env bash
# An old mail server of another make for the import test: Dovecot in a
# container on the host's network, IMAP with TLS on 127.0.0.1:9993, its
# certificate from a test CA (written to the directory given). Two
# accounts, olena@old.example and marek@old.example (password
# old-server-pass-2026), with mail in folders, flags and a keyword. Prints
# the settings for the test.
#
#   ci/outside-imap.sh <directory> | stop
set -euo pipefail
name=ortus-outside-imap
if [ "${1:-}" = stop ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  exit 0
fi
dir=$(realpath -m "${1:?a directory for the CA certificate}")
mkdir -p "$dir"
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --network host -e HOST_UID="$(id -u)" -v "$dir:/out" debian:bookworm-slim bash -c '
  set -e
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq && apt-get install -y -qq dovecot-imapd openssl >/dev/null
  # A test CA and the server certificate from it (a CA certificate is not
  # taken as a server certificate).
  cd /etc/dovecot
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out /out/ca.pem -days 2 -subj /CN=test-ca 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj /CN=localhost 2>/dev/null
  printf "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n" > server.ext
  openssl x509 -req -in server.csr -CA /out/ca.pem -CAkey ca.key -CAcreateserial -days 2 -extfile server.ext -out server.pem 2>/dev/null
  chmod 644 /out/ca.pem
  useradd -r -m vmail 2>/dev/null || true
  pass=$(doveadm pw -s SHA512-CRYPT -p old-server-pass-2026)
  printf "olena@old.example:%s\nmarek@old.example:%s\n" "$pass" "$pass" > /etc/dovecot/users
  cat > /etc/dovecot/local.conf <<CONF
protocols = imap
listen = 127.0.0.1
ssl = required
ssl_cert = </etc/dovecot/server.pem
ssl_key = </etc/dovecot/server.key
disable_plaintext_auth = yes
auth_mechanisms = plain login
passdb {
  driver = passwd-file
  args = scheme=SHA512-CRYPT /etc/dovecot/users
}
userdb {
  driver = static
  args = uid=vmail gid=vmail home=/var/mail/%d/%n
}
mail_location = maildir:~/Maildir:LAYOUT=fs
namespace inbox {
  inbox = yes
  separator = /
  mailbox Sent {
    special_use = \Sent
    auto = create
  }
}
service imap-login {
  inet_listener imap {
    port = 0
  }
  inet_listener imaps {
    port = 9993
    ssl = yes
  }
}
log_path = /dev/stderr
CONF
  mkdir -p /var/mail && chown vmail:vmail /var/mail
  msg() { printf "From: Anna Kowalska <anna@regional.example>\r\nTo: %s\r\nSubject: %s\r\nMessage-ID: <%s@regional.example>\r\nDate: Thu, 19 Nov 2026 08:05:00 +0000\r\n\r\nClause 4 is signed.\r\n" "$1" "$2" "$(echo "$2" | tr -dc a-zA-Z)"; }
  dovecot
  for i in $(seq 1 30); do doveadm user olena@old.example >/dev/null 2>&1 && break; sleep 1; done
  o=olena@old.example
  doveadm mailbox create -u $o Sent "Projects/2026" 2>/dev/null || true
  msg $o "Gauge report" | doveadm save -u $o -m INBOX
  msg $o "Shelter agreement" | doveadm save -u $o -m INBOX
  msg $o "Re: Gauge report" | doveadm save -u $o -m Sent
  msg $o "Bus plan" | doveadm save -u $o -m "Projects/2026"
  doveadm flags add -u $o "\\Seen flood" mailbox INBOX subject "Gauge report"
  doveadm flags add -u $o "\\Seen" mailbox Sent ALL
  doveadm flags add -u $o "\\Flagged" mailbox "Projects/2026" ALL
  msg marek@old.example "Escort schedule" | doveadm save -u marek@old.example -m INBOX
  touch /out/ready
  exec sleep infinity
' >/dev/null
for _ in $(seq 1 180); do
  [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ] || break
  if [ -e "$dir/ready" ] && (exec 3<>/dev/tcp/127.0.0.1/9993) 2>/dev/null; then
    echo "export ORTUS_TEST_DOVECOT=127.0.0.1:9993 ORTUS_TEST_DOVECOT_CA=$dir/ca.pem"
    exit 0
  fi
  sleep 1
done
docker logs "$name" >&2 || true
echo "outside-imap: Dovecot did not start" >&2
exit 1
