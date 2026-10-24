// fbrain plugin for opencode 1.x. Written by `fbrain install opencode-v1`; `fbrain uninstall opencode-v1` removes it.
import { spawn } from "node:child_process";

const FBRAIN = __FBRAIN_BIN__;
const NOT_LOADED = "fbrain memory not loaded: fbrain binary not found. Run fbrain doctor.";
const TIMEOUT_MS = 15000;
const MAX_BYTES = 64 * 1024;

// The binary is run directly (no shell) with stdin ignored. The result is the brief or the
// notice. The timer itself answers when time is up, so a process that ignores SIGTERM or a
// grandchild that holds the pipe open cannot hang the session; output past MAX_BYTES is dropped.
function brief(cwd) {
  return new Promise((resolve) => {
    const chunks = [];
    let size = 0;
    let child;
    let done = false;
    const finish = (text) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolve(text);
      if (child) {
        child.kill("SIGKILL");
        child.stdout?.destroy();
      }
    };
    const timer = setTimeout(() => finish(NOT_LOADED), TIMEOUT_MS);
    try {
      child = spawn(FBRAIN, ["brief", "--harness", "opencode-v1", "--cwd", cwd], {
        stdio: ["ignore", "pipe", "ignore"],
        windowsHide: true,
      });
    } catch {
      finish(NOT_LOADED);
      return;
    }
    child.stdout.on("data", (d) => {
      size += d.length;
      if (size > MAX_BYTES) finish(NOT_LOADED);
      else chunks.push(d);
    });
    child.on("error", () => finish(NOT_LOADED));
    child.on("close", (code) => {
      const out = Buffer.concat(chunks).toString("utf8");
      finish(code === 0 && out.trim() ? out.trimEnd() : NOT_LOADED);
    });
  });
}

export const fbrain = async ({ directory } = {}) => {
  const cwd = directory || process.cwd();
  const sessions = new Map();
  return {
    // Every shell command opencode runs gets the marker that tells `fbrain` an agent runs it.
    "shell.env": async (_input, output) => {
      output.env.FBRAIN_HARNESS = "opencode";
    },
    // Runs on every model call, so the brief is cached per session; a call without a session id is not cached.
    "experimental.chat.system.transform": async (input, output) => {
      const id = input?.sessionID;
      let text;
      if (id === undefined) {
        text = await brief(cwd);
      } else {
        if (!sessions.has(id)) sessions.set(id, brief(cwd));
        text = await sessions.get(id);
      }
      output.system.push(text);
    },
  };
};
