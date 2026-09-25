// MuseDesk frontend — talks to the Rust backend via Tauri invoke/events.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const messagesEl = document.getElementById("messages");
const inputEl = document.getElementById("input");
const sendBtn = document.getElementById("send");

let streamingEl = null;

function addMsg(cls, html) {
  const div = document.createElement("div");
  div.className = "msg " + cls;
  div.innerHTML = html;
  messagesEl.appendChild(div);
  messagesEl.scrollTop = messagesEl.scrollHeight;
  return div;
}
function esc(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

async function send() {
  const text = inputEl.value.trim();
  if (!text) return;
  inputEl.value = "";
  sendBtn.disabled = true;
  addMsg("user", esc(text));
  streamingEl = addMsg("agent", "");
  document.body.classList.add("chatting");
  try {
    await invoke("send_message", { text });
  } catch (e) {
    addMsg("error", "Failed to start agent: " + esc(String(e)));
    sendBtn.disabled = false;
  }
}

sendBtn.addEventListener("click", send);
inputEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
});

listen("token", (e) => {
  if (streamingEl) {
    streamingEl.textContent += e.payload;
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }
});

listen("tool-call", (e) => {
  streamingEl = null;
  const { name, arguments: args } = e.payload;
  addMsg("tool", `<span class="tname">⚙ ${esc(name)}</span>\n${esc(JSON.stringify(args, null, 2))}`);
});

listen("tool-result", (e) => {
  const { result } = e.payload;
  addMsg("tool", `<span class="tname">↩ result</span>\n${esc(String(result)).slice(0, 2000)}`);
});

listen("approval-requested", async (e) => {
  const { id, tool, arguments: args } = e.payload;
  const div = document.createElement("div");
  div.className = "approval";
  div.innerHTML = `<p>Approve <b>${esc(tool)}</b>?</p>` +
    `<code>${esc(JSON.stringify(args, null, 2))}</code>`;
  const yes = document.createElement("button");
  yes.className = "yes"; yes.textContent = "Approve";
  const no = document.createElement("button");
  no.className = "no"; no.textContent = "Deny";
  const done = async (approved) => {
    yes.disabled = no.disabled = true;
    await invoke("resolve_approval", { id, approved });
    div.querySelector("p").innerHTML = approved
      ? "Approved ✓" : "Denied ✗";
  };
  yes.onclick = () => done(true);
  no.onclick = () => done(false);
  div.appendChild(yes); div.appendChild(no);
  messagesEl.appendChild(div);
  messagesEl.scrollTop = messagesEl.scrollHeight;
});

listen("done", () => { streamingEl = null; sendBtn.disabled = false; });
listen("agent-error", (e) => {
  streamingEl = null; sendBtn.disabled = false;
  addMsg("error", esc(String(e.payload)));
});

// Suggestion pills reuse the existing send flow: fill the composer and send.
document.querySelectorAll("#sugg button").forEach((pill) => {
  pill.addEventListener("click", () => {
    inputEl.value = pill.dataset.prompt;
    send();
  });
});

// New chat is frontend-only: clear the view and restore the welcome state.
document.getElementById("newchat").addEventListener("click", () => {
  messagesEl.innerHTML = "";
  streamingEl = null;
  sendBtn.disabled = false;
  inputEl.value = "";
  document.body.classList.remove("chatting");
  inputEl.focus();
});
