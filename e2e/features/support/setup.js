// Stack lifecycle for the cucumber e2e: the bash harness's setup/teardown,
// reused verbatim. run.sh stays the single source of truth for infrastructure
// (image, network, tokens, containers, real ssh arm): we run it with TESTS=0
// so it sets up and LEAVES THE STACK RUNNING (no cleanup), then cucumber
// drives the presses and asserts. Cleanup here, on exit, always.

const { execFileSync } = require("node:child_process");
const path = require("node:path");
const { BeforeAll, AfterAll } = require("@cucumber/cucumber");

// setup.js lives in e2e/features/support/ -> repo root is three levels up.
const REPO = path.resolve(__dirname, "..", "..", "..");
const HUB = "mm-c", A = "mm-a", B = "mm-b";
const NET = "muxmesh-e2e-net";

// cucumber v12: no this.timeout() in hooks — timeout is a hook option (ms).
// The stack runs ONCE for the whole suite: scenarios are one continuous
// story (press 1 -> delegate -> press 2 -> reclaim) on live containers.
BeforeAll({ timeout: 20 * 60 * 1000 }, () => {
  // full cargo build inside the image can take minutes
  execFileSync("bash", [path.join(REPO, "e2e", "run.sh")], {
    cwd: REPO, stdio: "inherit", env: { ...process.env, TESTS: "0", KEEP: "1", SKIP_BUILD: process.env.SKIP_BUILD },
  });
});

AfterAll(() => {
  if (process.env.KEEP === "1") { console.log("KEEP=1: stack left running"); return; }
  execFileSync("bash", ["-c", `docker rm -f ${HUB} ${A} ${B} >/dev/null 2>&1; docker network rm ${NET} >/dev/null 2>&1 || true`], {
    stdio: "inherit",
  });
});
