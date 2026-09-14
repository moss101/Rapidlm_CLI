import { RapidClient } from "/Users/mohsin/projects/RapidLM CLI/sdk/typescript/dist/index.js";
import { spawn } from "node:child_process";

const socket = "/tmp/dk-smoke/.rapidlm/daemon.sock";
const daemon = spawn("/Users/mohsin/projects/RapidLM CLI/target/debug/rapid", ["daemon"], {
  cwd: "/tmp/dk-smoke", env: { ...process.env, HOME: "/tmp/dk-home" }, stdio: ["ignore", "pipe", "pipe"],
});
let daemonOut = "";
daemon.stdout.on("data", (d) => (daemonOut += d));
daemon.stderr.on("data", (d) => (daemonOut += d));
for (let i = 0; i < 60; i++) { await new Promise((r) => setTimeout(r, 250)); if (daemonOut.includes("listening")) break; }
if (!daemonOut.includes("listening")) { console.log("DAEMON OUTPUT:", daemonOut); process.exit(1); }
console.log("[1] daemon listening");

const client = await RapidClient.connect({ transport: "local", socketPath: socket, authToken: "smoke-token" });
console.log("[2] connected:", client.connected);
const session = await client.sessions.create({ project: "/tmp/dk-smoke" });
console.log("[3] sessions.create ->", session.id, "seq", session.seq);

// submit + stream a real turn (unconfigured model -> the turn honestly fails; the stream still delivers)
const events = [];
const run = (async () => { try { for await (const ev of session.run({ prompt: "hello from the sdk" })) { events.push(ev.kind); } } catch (e) { console.log("[4] stream ended:", e.code ?? e.message); } })();
await new Promise((r) => setTimeout(r, 3000));
console.log("[4] streamed", events.length, "event(s):", events.slice(0, 8).join(", "));
await run;

// reconnect + resume: fresh client, same session, subscribe from cursor 0
const client2 = await RapidClient.connect({ transport: "local", socketPath: socket, authToken: "smoke-token" });
const forked = await session.fork();
console.log("[5] sessions.fork ->", forked.id, "(a distinct session derived from", session.id + ")");
const resumedEvents = [];
const timeout = setTimeout(() => { console.log("[6] TIMEOUT waiting for replay"); process.exit(1); }, 8000);
for await (const ev of forked.subscribe({ from_seq: 0 })) { resumedEvents.push(ev.kind); if (resumedEvents.length >= 1) break; }
clearTimeout(timeout);
console.log("[6] resume replayed", resumedEvents.length, "replayed event(s):", resumedEvents.slice(0, 4).join(", "), "…");
await client2.close();
daemon.kill();
console.log("[7] done — real SDK client session over the daemon socket");
process.exit(0);
