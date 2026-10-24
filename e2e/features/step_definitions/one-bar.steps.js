// Step definitions for one-bar.feature. Each step is a verbatim assertion or
// action from the bash harness (e2e/run.sh), driven with docker exec.
// The stack itself (image, network, tokens, containers, ssh arm) is brought
// up once per run by support/setup.js — the scenarios only assert and press.

const { execFileSync } = require("node:child_process");
const { Given, When, Then } = require("@cucumber/cucumber");

const A = "mm-a", B = "mm-b", HUB = "mm-c";
const SOCK_A = "/tmp/tmux-a.sock", SOCK_B = "/tmp/tmux-b.sock";
const HOST_A = "machine_a", HOST_B = "machine_b";

function sh(container, cmd) {
  return execFileSync("docker", ["exec", container, "sh", "-c", cmd], {
    encoding: "utf8",
  }).trim();
}

// the F10 keystroke, exactly as the tmux binding fires it.
function pressF10(container, host, sock) {
  // MUXMESH_IPC_SOCKET: the agent binds ~/.muxmesh/agent.sock; docker exec
  // does not always carry HOME, so point the CLI at the socket explicitly.
  const env = `MUXMESH_HOST=${host} MUXMESH_SOCKET=${sock} MUXMESH_IPC_SOCKET=/root/.muxmesh/agent.sock MUXMESH_POLL_HUB=mm-c:9997 HOME=/root`;
  const out = execFileSync(
    "docker",
    ["exec", container, "sh", "-c", `${env} muxmesh-cli bar-toggle -v`],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  return out;
}

// the hub directory via the agent's IPC socket — did the CLI see the world?
function pressTrace(container) {
  // best-effort diagnostic only: a missing log must never mask the real error.
  try {
    return execFileSync(
      "docker", ["exec", container, "tail", "-5", "/root/.muxmesh/bar-toggle.log"],
      { encoding: "utf8" },
    );
  } catch { return "(no bar-toggle.log)"; }
}

const bar = (c, sock) => sh(c, `tmux -S ${sock} show-options -g status | awk '{print $2}'`);
const prefix = (c, sock) => sh(c, `tmux -S ${sock} show-options -gv prefix`);
const winCount = (c, sock) => Number(sh(c, `tmux -S ${sock} list-windows -a | wc -l | tr -d ' '`));

Then("machine_a holds its prefix (not None)", () => {
  if (prefix(A, SOCK_A) === "None") throw new Error("machine_a prefix already None");
});

Then("both bars are visible before the test", () => {
  if (bar(A, SOCK_A) !== "on") throw new Error("machine_a bar not on");
  if (bar(B, SOCK_B) !== "on") throw new Error("machine_b bar not on");
});

When("I press F10 on machine_a", () => {
  const out = pressF10(A, HOST_A, SOCK_A);
  if (out.includes("no agent") || out.includes("agent/hub silent"))
    throw new Error(`F10 on machine_a: agent path deaf — ${out.trim()}\nlog:\n${pressTrace(A)}`);
});
When("I press F10 on machine_b", () => {
  const out = pressF10(B, HOST_B, SOCK_B);
  if (out.includes("no agent") || out.includes("agent/hub silent"))
    throw new Error(`F10 on machine_b: agent path deaf — ${out.trim()}\nlog:\n${pressTrace(B)}`);
});

Then("machine_a's bar is hidden", () => {
  if (bar(A, SOCK_A) !== "off") throw new Error(`machine_a bar = ${bar(A, SOCK_A)}, want off`);
});

Then("machine_b's bar is shown", () => {
  if (bar(B, SOCK_B) !== "on") throw new Error(`machine_b bar = ${bar(B, SOCK_B)}, want on`);
});

Then("machine_a's control is delegated (prefix None)", () => {
  if (prefix(A, SOCK_A) !== "None") throw new Error(`prefix = ${prefix(A, SOCK_A)}, want None`);
});

Then(/^machine_a's client sits in the deaf key table "([^"]*)"$/, (table) => {
  const got = sh(A, `tmux -S ${SOCK_A} display-message -p '#{client_key_table}'`);
  if (got !== table) throw new Error(`key table = '${got}', want '${table}'`);
});

Then(/^exactly one bar is visible: machine_a (on|off), machine_b (on|off)$/, (a, b) => {
  const ga = bar(A, SOCK_A), gb = bar(B, SOCK_B);
  if (ga !== a || gb !== b) throw new Error(`bars A=${ga} B=${gb}, want A=${a} B=${b}`);
});

When(/^a window is created on (machine_a|machine_b)$/, (host) => {
  const [c, sock] = host === "machine_a" ? [A, SOCK_A] : [B, SOCK_B];
  global.__before = { a: winCount(A, SOCK_A), b: winCount(B, SOCK_B) };
  sh(c, host === "machine_a" ? `tmux -S ${sock} new-window` : `tmux -S ${sock} new-window -t main:`);
});

Then(/^machine_(a|b) has one more window and machine_(a|b) is unchanged$/, (grow, stable) => {
  const after = { a: winCount(A, SOCK_A), b: winCount(B, SOCK_B) };
  const before = global.__before;
  if (after[grow] !== before[grow] + 1)
    throw new Error(`machine_${grow}: ${before[grow]} -> ${after[grow]}, want +1`);
  if (after[stable] !== before[stable])
    throw new Error(`machine_${stable} moved: ${before[stable]} -> ${after[stable]}, want stable`);
});

Then("machine_b's bar is hidden", () => {
  if (bar(B, SOCK_B) !== "off") throw new Error(`machine_b bar = ${bar(B, SOCK_B)}, want off`);
});

Then("machine_a's bar is restored by the hub (ResumeControl)", () => {
  if (bar(A, SOCK_A) !== "on") throw new Error(`machine_a bar = ${bar(A, SOCK_A)}, want on`);
});

Then("machine_a's control is reclaimed (prefix back, F10 alive in root)", () => {
  if (prefix(A, SOCK_A) === "None") throw new Error("prefix still None — parent stuck");
  if (!sh(A, `tmux -S ${SOCK_A} list-keys -T root`).includes("F10"))
    throw new Error("F10 missing from root — parent deaf forever");
});

Given("the muxmesh stack is running", () => {
  // setup.js BeforeAll brought it up; cheap liveness check, no re-setup.
  const running = execFileSync("docker", ["ps", "--format", "{{.Names}}"], { encoding: "utf8" });
  for (const c of [HUB, A, B])
    if (!running.includes(c)) throw new Error(`stack container ${c} not running`);
});
Given("machine_a is forced into the delegated state (prefix None, bar off)", () => {
  sh(A, `tmux -S ${SOCK_A} set-option -g prefix None`);
  sh(A, `tmux -S ${SOCK_A} set-option -g status off`);
});

Then("machine_a's prefix is restored (self-reclaim)", () => {
  if (prefix(A, SOCK_A) === "None") throw new Error("prefix reste None — F10 MORT");
});

Then("machine_a's F10 is alive in the root key table", () => {
  if (!sh(A, `tmux -S ${SOCK_A} list-keys -T root`).includes("F10"))
    throw new Error("F10 absent de root — sourd pour toujours");
});

Then("machine_a's bar is back on", () => {
  if (bar(A, SOCK_A) !== "on") throw new Error(`machine_a bar = ${bar(A, SOCK_A)}, want on`);
});
