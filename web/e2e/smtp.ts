// A small SMTP client for the tests: hands one message to the program's
// receiving port, as another mail server would.
import { Socket } from "node:net";

export async function deliver(port: number, from: string, to: string, message: string): Promise<string> {
  const socket = new Socket();
  await new Promise<void>((resolve, reject) => socket.connect(port, "127.0.0.1", resolve).once("error", reject));
  let buffer = "";
  const reply = (): Promise<string> =>
    new Promise((resolve) => {
      const check = () => {
        const lines = buffer.split("\r\n");
        const end = lines.findIndex((l) => /^\d{3} /.test(l));
        if (end >= 0) {
          const text = lines.slice(0, end + 1).join("\n");
          buffer = lines.slice(end + 1).join("\r\n");
          socket.off("data", onData);
          resolve(text);
        }
      };
      const onData = (chunk: Buffer) => {
        buffer += chunk.toString("utf8");
        check();
      };
      socket.on("data", onData);
      check();
    });
  const say = async (line: string) => {
    socket.write(`${line}\r\n`);
    return reply();
  };
  await reply();
  await say("EHLO google.com");
  await say(`MAIL FROM:<${from}>`);
  const rcpt = await say(`RCPT TO:<${to}>`);
  if (!rcpt.startsWith("250")) throw new Error(rcpt);
  await say("DATA");
  const body = message.replace(/\r?\n/g, "\r\n").split("\r\n").map((l) => (l.startsWith(".") ? `.${l}` : l)).join("\r\n");
  const done = await say(`${body}\r\n.`);
  await say("QUIT");
  socket.destroy();
  return done;
}
