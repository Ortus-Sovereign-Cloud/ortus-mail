// A test-only authoritative DNS server for the browser tests: answers from
// e2e/zone.json (read on every query, so tests change it as they go), on
// UDP and TCP at the port given.
import dgram from "node:dgram";
import net from "node:net";
import { readFileSync } from "node:fs";
import packet from "dns-packet";

const port = Number(process.argv[2] ?? 5399);
const zoneFile = new URL("./zone.json", import.meta.url);

function answer(query) {
  let zone = {};
  try {
    zone = JSON.parse(readFileSync(zoneFile, "utf8"));
  } catch {
    // no zone yet
  }
  const question = query.questions?.[0];
  const key = `${question.name.toLowerCase().replace(/\.$/, "")} ${question.type}`;
  const answers = (zone[key] ?? []).map((data) => ({ name: question.name, type: question.type, ttl: 60, data }));
  return packet.encode({ id: query.id, type: "response", flags: packet.AUTHORITATIVE_ANSWER | (answers.length ? 0 : 3), questions: query.questions, answers });
}

const udp = dgram.createSocket("udp4");
udp.on("message", (message, from) => {
  try {
    udp.send(answer(packet.decode(message)), from.port, from.address);
  } catch {
    // a broken query gets no answer
  }
});
udp.bind(port, "127.0.0.1");

net
  .createServer((socket) => {
    let buffer = Buffer.alloc(0);
    socket.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      while (buffer.length >= 2 && buffer.length >= 2 + buffer.readUInt16BE(0)) {
        const length = buffer.readUInt16BE(0);
        const reply = answer(packet.decode(buffer.subarray(2, 2 + length)));
        buffer = buffer.subarray(2 + length);
        const size = Buffer.alloc(2);
        size.writeUInt16BE(reply.length);
        socket.write(Buffer.concat([size, reply]));
      }
    });
  })
  .listen(port, "127.0.0.1", () => console.log(`dns: listening on ${port}`));
