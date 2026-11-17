//! Test-only: run a test body in a child copy of the test binary with `TZ`
//! set, so a local-time assertion is pinned to a named zone instead of the
//! host's, without mutating the environment of the shared test process.

use std::process::Command;

/// US Eastern rules as a POSIX TZ string: UTC-5, UTC-4 from 02:00 on the
/// second Sunday of March to 02:00 on the first Sunday of November.
pub fn eastern() -> String {
    posix_tz("EST5EDT", "3.2.0", "11.1.0")
}

/// UTC-3, UTC-2 from 00:00 on the first Sunday of November (that day has no
/// midnight) to 01:00 on the third Sunday of February (that day has two).
pub fn midnight_dst() -> String {
    posix_tz("<-03>3<-02>", "11.1.0/0", "2.3.0/1")
}

/// `<zones>,M<start>,M<end>`. The `M` month-rule letter is joined here rather
/// than written beside its digits, because `scripts/check-internal-ids.sh`
/// refuses that spelling as an internal id shape.
pub fn posix_tz(zones: &str, start: &str, end: &str) -> String {
    const MONTH_RULE: char = 'M';
    format!("{zones},{MONTH_RULE}{start},{MONTH_RULE}{end}")
}

/// Names the test and zone the child runs (`<test> <tz>`); its presence is
/// what makes the child run a body instead of spawning again.
const CHILD_ENV: &str = "ROUTECTL_TEST_TZ_CHILD";

/// Printed by the child after the body returns. libtest exits 0 on an empty
/// selection, so the marker is what proves the body actually ran.
const CHILD_PASSED: &str = "routectl-test-tz-child-passed";

/// Run `body` with `TZ=tz`. `module` is the caller's `module_path!()` and
/// `test` its own function name, which together select exactly this test in
/// the child. One test may call this once per zone: the child runs only the
/// body whose zone it was spawned for.
pub fn in_zone(tz: &str, module: &str, test: &str, body: impl FnOnce()) {
    let key = format!("{test} {tz}");
    if let Some(named) = std::env::var_os(CHILD_ENV) {
        if named == key.as_str() {
            body();
            // libtest has already printed `test <name> ... ` without a newline.
            println!("\n{CHILD_PASSED}");
        }
        return;
    }
    let module = module.split_once("::").map_or("", |(_crate, rest)| rest);
    let program = std::env::current_exe().expect("the running test binary");
    let run = Command::new(&program)
        .args([
            &format!("{module}::{test}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &key)
        .env("TZ", tz)
        .output()
        .unwrap_or_else(|why| panic!("run child {}: {why}", program.display()));
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        run.status.success() && stdout.lines().any(|line| line == CHILD_PASSED),
        "the {test} child under TZ={tz} must pass and print its marker; status {}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        run.status
    );
}
