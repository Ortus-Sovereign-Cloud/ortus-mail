#!/usr/bin/env bash
# An outside mail server for tests: Postfix in a container on the host's
# network, listening on 127.0.0.1:2525. It delivers mail for
# outside.example to bob's Maildir (in the directory given, shared with the
# test), and hands mail for ministry.example to 127.0.0.1:2526, where the
# test's Ortus listens. Prints the settings for the test.
#
#   ci/outside-mail.sh <maildir> | stop
set -euo pipefail
name=ortus-outside-mail
if [ "${1:-}" = stop ]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  exit 0
fi
dir=$(realpath -m "${1:?a directory for the Maildir}")
mkdir -p "$dir"
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" --network host -e HOST_UID="$(id -u)" -v "$dir:/home/bob/Maildir" debian:stable-slim bash -c '
  set -e
  export DEBIAN_FRONTEND=noninteractive
  echo "postfix postfix/main_mailer_type select No configuration" | debconf-set-selections
  apt-get update -qq && apt-get install -y -qq postfix >/dev/null
  # Bob has the uid of whoever runs the tests, who then reads his Maildir.
  useradd -m -u "$HOST_UID" -o bob 2>/dev/null || true
  chown -R bob /home/bob/Maildir
  cp /usr/share/postfix/main.cf.debian /etc/postfix/main.cf
  postconf -e "myhostname = mail.outside.example" "mydomain = outside.example" \
    "mydestination = outside.example, localhost" "inet_interfaces = 127.0.0.1" "inet_protocols = ipv4" \
    "mynetworks = 127.0.0.0/8" "home_mailbox = Maildir/" "transport_maps = hash:/etc/postfix/transport" \
    "smtp_tls_security_level = none" "smtpd_tls_security_level = none" "maillog_file = /dev/stdout" \
    "smtpd_banner = mail.outside.example ESMTP" "relay_domains =" "compatibility_level = 3.6"
  postconf -MX smtp/inet
  postconf -M "2525/inet=2525 inet n - n - - smtpd"
  echo "ministry.example smtp:[127.0.0.1]:2526" > /etc/postfix/transport
  postmap /etc/postfix/transport
  touch /etc/aliases && newaliases
  exec postfix start-fg
' >/dev/null
for _ in $(seq 1 120); do
  if (exec 3<>/dev/tcp/127.0.0.1/2525) 2>/dev/null; then
    echo "export ORTUS_TEST_OUTSIDE_MAILDIR=$dir"
    exit 0
  fi
  sleep 1
done
docker logs "$name" >&2 || true
echo "outside-mail: Postfix did not start" >&2
exit 1
