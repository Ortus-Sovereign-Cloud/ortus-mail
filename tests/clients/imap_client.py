"""A real IMAP client (Python's imaplib) against Ortus: what Thunderbird
and phones do, step by step. Arguments: port, CA file, address, password."""
import imaplib
import ssl
import sys
import time

port, cafile, user, password = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
context = ssl.create_default_context(cafile=cafile)


def check(ok, what):
    if not ok:
        print("FAILED:", what)
        sys.exit(1)


def open_imap():
    m = imaplib.IMAP4_SSL("127.0.0.1", port, ssl_context=context)
    return m


# The certificate is for mail.ministry.example; connect by name through 127.0.0.1.
context.check_hostname = False
m = open_imap()
check("IDLE" in m.capabilities and "AUTH=PLAIN" in m.capabilities and "SASL-IR" in m.capabilities, m.capabilities)
try:
    m.login(user, "wrong-password-0000")
    check(False, "a wrong password signed in")
except imaplib.IMAP4.error:
    pass
# AUTHENTICATE PLAIN with a continuation, as Thunderbird does.
a = open_imap()
typ, _ = a.authenticate("PLAIN", lambda _: f"\0{user}\0{password}".encode())
check(typ == "OK", "authenticate")
a.logout()
m = open_imap()
typ, _ = m.login(user, password)
check(typ == "OK", "login")

typ, boxes = m.list()
names = b"\n".join(boxes).decode()
check("INBOX" in names and "\\Sent" in names and "\\Junk" in names and "\\Trash" in names, names)

typ, data = m.select("INBOX")
check(typ == "OK" and int(data[0]) == 2, f"select {data}")
typ, data = m.search(None, "UNSEEN")
check(data[0].split() == [b"1", b"2"], f"unseen {data}")
typ, data = m.search(None, "FROM", '"maria"')
check(data[0].split() == [b"1"], f"from {data}")
typ, data = m.search(None, "SUBJECT", '"budget"')
check(data[0].split() == [b"2"], f"subject {data}")

typ, data = m.fetch("1:*", "(UID FLAGS RFC822.SIZE ENVELOPE BODYSTRUCTURE)")
text = b" ".join(p if isinstance(p, bytes) else p[0] for p in data).decode()
check('"Flood damage report"' in text and '"maria" "mail.example"' in text, text)
check('("TEXT" "PLAIN"' in text and '"MIXED"' in text, text)

typ, data = m.fetch("1", "(BODY.PEEK[HEADER.FIELDS (SUBJECT FROM)])")
check(b"Subject: Flood damage report" in data[0][1] and b"To:" not in data[0][1], data)
typ, data = m.fetch("1", "(FLAGS)")
check(b"\\Seen" not in data[0], f"peek left it unread {data}")
typ, data = m.fetch("2", "(BODY[1])")
check(b"Numbers attached" in data[0][1], data)
typ, data = m.fetch("2", "(FLAGS)")
check(b"\\Seen" in data[0], f"read marks seen {data}")
typ, data = m.fetch("1", "(BODY[]<0.4>)")
check(data[0][1] == b"From", data)

typ, data = m.store("1", "+FLAGS", "(\\Flagged)")
check(b"\\Flagged" in data[0], data)
typ, data = m.uid("SEARCH", None, "FLAGGED")
flagged_uid = data[0].split()[0]

typ, data = m.uid("COPY", flagged_uid, "Archive")
check(typ == "OK", f"copy {data}")
typ, data = m.create("Projects/2026")
check(typ == "OK", f"create {data}")
typ, data = m.append("Projects/2026", "(\\Seen)", imaplib.Time2Internaldate(time.time()),
                     b"From: olena@ministry.example\r\nTo: sam@ministry.example\r\nSubject: Notes\r\nMessage-ID: <n1@ministry.example>\r\nDate: Fri, 20 Nov 2026 09:00:00 +0000\r\n\r\nPlan for the week.\r\n")
check(typ == "OK" and b"APPENDUID" in data[0], f"append {data}")
typ, data = m.status("Projects/2026", "(MESSAGES UNSEEN)")
check(b"MESSAGES 1" in data[0] and b"UNSEEN 0" in data[0], data)
typ, data = m.rename("Projects/2026", "Projects/Done")
check(typ == "OK", f"rename {data}")
try:
    typ, data = m.rename("INBOX", "Elsewhere")
    check(typ != "OK", "renamed INBOX")
except imaplib.IMAP4.error:
    pass

# IDLE: a second session adds a message; the first hears of it.
m.send(b"i1 IDLE\r\n")
check(m.readline().startswith(b"+"), "idle continuation")
other = open_imap()
other.login(user, password)
other.append("INBOX", None, None, b"From: sam@ministry.example\r\nTo: olena@ministry.example\r\nSubject: Lunch\r\nMessage-ID: <l1@ministry.example>\r\n\r\nNoon?\r\n")
heard = m.readline()
check(heard.strip() == b"* 3 EXISTS", f"idle heard {heard}")
m.send(b"DONE\r\n")
done = m.readline()
check(done.startswith(b"i1 OK"), f"idle done {done}")
other.logout()

# Delete and expunge; MOVE takes it from INBOX.
typ, data = m.uid("MOVE", flagged_uid, "Trash")
check(typ == "OK", f"move {data}")
typ, data = m.select("INBOX")
check(int(data[0]) == 2, f"after move {data}")
m.store("1", "+FLAGS", "(\\Deleted)")
typ, data = m.expunge()
check(data == [b"1"], f"expunge {data}")
typ, data = m.select("Trash", readonly=True)
check(int(data[0]) == 1, f"trash {data}")
typ, data = m.delete("Projects/Done")
check(typ == "OK", f"delete {data}")
m.logout()
print("imap client: all steps passed")
