// Stack lifecycle for the cucumber e2e: the bash harness's setup/teardown,
// reused verbatim. run.sh stays the single source of truth for infrastructure
// (image, network, tokens, containers, real ssh arm): we run it with TESTS=0
// so it sets up and LEAVES THE STACK RUNNING (no cleanup), then cucumber
// drives the presses and asserts. Cleanup here, on exit, always.

const { execFileSync } = require("node:child_process");
const path = require("node:path");
const { BeforeAll, AfterAll, Before } = require("@cucumber/cucumber");

// setup.js lives in e2e/features/support/ -> repo root is three levels up.
const REPO = path.resolve(__dirname, "..", "..", "..");
const HUB = "mm-c", A = "mm-a", B = "mm-b";
const NET = "muxmesh-e2e-net";
const SOCK_A = "/tmp/tmux-a.sock", SOCK_B = "/tmp/tmux-b.sock";

// etat de reference du protocole : LOCAL = les deux barres on, prefix
// parent intact. Tout scenario qui presse doit repartir de cet etat ;
// un etat herite (delegate) inverse le role du press suivant.
function resetToLocal() {
  const sh = (c, cmd) => execFileSync("docker", ["exec", c, "sh", "-c", cmd], { encoding: "utf8" }).trim();
  sh(A, `tmux -S ${SOCK_A} set-option -gu prefix`);
  sh(A, `tmux -S ${SOCK_A} set-option -g status on`);
  sh(B, `tmux -S ${SOCK_B} set-option -gu prefix`);
  sh(B, `tmux -S ${SOCK_B} set-option -g status on`);
  // l'etat delegue est GLOBAL (key-table off) : le unset ramene tout le
  // serveur a root ; refresh repeint la barre. mm-b headless : normal.
  sh(A, `tmux -S ${SOCK_A} set-option -gu key-table`);
  sh(B, `tmux -S ${SOCK_B} set-option -gu key-table`);
  try { sh(A, `tmux -S ${SOCK_A} refresh-client -S`); } catch { /* no client */ }
  try { sh(B, `tmux -S ${SOCK_B} refresh-client -S`); } catch { /* headless */ }
  // le scenario "fenetre chez l'enfant" cree une fenetre sur mm-b : la
  // fermer ici pour que les compteurs repartent a zero (le scenario attend +1).
  try { sh(B, `tmux -S ${SOCK_B} kill-window -t main:1 2>/dev/null; true`); } catch { /* nothing to kill */ }
}

// cucumber v12: no this.timeout() in hooks — timeout is a hook option (ms).
// The stack runs ONCE for the whole suite: scenarios are one continuous
// story (press 1 -> delegate -> press 2 -> reclaim) on live containers.
BeforeAll({ timeout: 20 * 60 * 1000 }, () => {
  // SKIP_SETUP=1 : stack deja montee avec binaires injectes, ne pas relancer run.sh.
  if (process.env.SKIP_SETUP === "1") { console.log("  SKIP_SETUP=1: stack reusee"); return; }
  // full cargo build inside the image can take minutes
  execFileSync("bash", [path.join(REPO, "e2e", "run.sh")], {
    cwd: REPO, stdio: "inherit", env: { ...process.env, TESTS: "0", KEEP: "1", SKIP_BUILD: process.env.SKIP_BUILD },
  });
});

Before({ timeout: 30 * 1000 }, (scenario) => {
  // etat LOCAL par defaut : prefix intact, barres on. Le scenario self-reclaim
  // force lui-meme prefix None + bar off (c'est son Given) — ne pas l'écraser.
  if (scenario.pickle.name.includes("self-reclaim")) return;
  // le scenario de delegation presse lui-meme : garder l'etat herite du
  // precedent (delegate) pour que sa presse soit bien la PRESSE 1 attendue.
  if (scenario.pickle.name.includes("delegates control")) return;
  // les scenarios de presse2 attendent l'etat DELEGUE herite du press1
  // (presse2 = toggle local chez l'enfant + ResumeControl vers le parent) ;
  // resetToLocal ferait de leur presse un simple toggle local on, sans effet.
  if (scenario.pickle.name.includes("press 2")) return;
  // les scenarios "apres press N" sont des assertions d'etat pur : ils heritent
  // de l'etat laisse par le press precedent (resetToLocal les effacerait).
  if (/^after (press|reclaim)/.test(scenario.pickle.name)) return;
  resetToLocal();
});

AfterAll(() => {
  if (process.env.KEEP === "1") { console.log("KEEP=1: stack left running"); return; }
  execFileSync("bash", ["-c", `docker rm -f ${HUB} ${A} ${B} >/dev/null 2>&1; docker network rm ${NET} >/dev/null 2>&1 || true`], {
    stdio: "inherit",
  });
});
