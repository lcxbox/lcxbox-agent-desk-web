//! Runs the process sandbox against stand-in services served by this test
//! binary itself: `main` dispatches a sandbox child before running any check.
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use desk_utils::process_sandbox::{
    self, FrameLimits, MEMORY_LIMIT_ENFORCED, SandboxFailure, SandboxSpec, Service,
    UNKNOWN_SERVICE_EXIT_CODE,
};
use serde::{Deserialize, Serialize};

const FRAMES: FrameLimits = FrameLimits {
    header_bytes: 4096,
    payload_bytes: 1024 * 1024,
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Message {
    value: String,
}

fn request(input: &mut dyn Read) -> io::Result<(Message, Vec<u8>)> {
    process_sandbox::read_frame(input, FRAMES)
}

fn echo(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    let (message, payload) = request(input)?;
    process_sandbox::write_frame(output, &message, &payload, FRAMES)
}

fn environment(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    let value = format!(
        "{}|{}",
        std::env::var("SANDBOX_TEST_VALUE").unwrap_or_default(),
        std::env::var("MALLOC_ARENA_MAX").unwrap_or_default()
    );
    process_sandbox::write_frame(output, &Message { value }, &[], FRAMES)
}

fn allocate(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    let mut block = vec![0_u8; 1024 * 1024 * 1024];
    for index in (0..block.len()).step_by(4096) {
        block[index] = 1;
    }
    let value = block
        .iter()
        .map(|byte| *byte as usize)
        .sum::<usize>()
        .to_string();
    process_sandbox::write_frame(output, &Message { value }, &[], FRAMES)
}

fn spin(input: &mut dyn Read, _: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    let mut counter = 0_u64;
    loop {
        counter = std::hint::black_box(counter.wrapping_add(1));
    }
}

fn abort(input: &mut dyn Read, _: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    std::process::abort()
}

#[allow(unconditional_recursion)]
fn recurse(depth: u64) -> u64 {
    let frame = std::hint::black_box([depth; 64]);
    recurse(frame[0] + 1) + frame[63]
}

fn overflow(input: &mut dyn Read, _: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    recurse(0);
    Ok(())
}

fn oversized(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    output.write_all(&u32::MAX.to_le_bytes())?;
    output.flush()
}

/// Answers with its process id, then keeps running until it is killed.
fn answer_then_spin(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    request(input)?;
    let value = std::process::id().to_string();
    process_sandbox::write_frame(output, &Message { value }, &[], FRAMES)?;
    spin_forever()
}

fn spin_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(10));
    }
}

const SERVICES: &[Service] = &[
    ("echo", echo),
    ("environment", environment),
    ("allocate", allocate),
    ("spin", spin),
    ("abort", abort),
    ("overflow", overflow),
    ("oversized", oversized),
    ("answer_then_spin", answer_then_spin),
];

fn spec() -> SandboxSpec {
    SandboxSpec {
        memory_bytes: 256 * 1024 * 1024,
        cpu_seconds: 10,
        wall_clock: Duration::from_secs(10),
        frames: FRAMES,
        env: &[("SANDBOX_TEST_VALUE", "configured")],
    }
}

fn run(service: &str, spec: &SandboxSpec) -> Result<(Message, Vec<u8>), SandboxFailure> {
    process_sandbox::run(
        service,
        &Message {
            value: "request".into(),
        },
        b"payload",
        spec,
    )
}

fn main() {
    if let Some(code) = process_sandbox::run_child_if_requested(SERVICES) {
        std::process::exit(code);
    }
    let checks: &[(&str, fn())] = &[
        ("a_request_round_trips", a_request_round_trips),
        (
            "extra_environment_reaches_the_child",
            extra_environment_reaches_the_child,
        ),
        (
            "an_unregistered_service_is_unavailable",
            an_unregistered_service_is_unavailable,
        ),
        (
            "a_crash_or_overflow_is_an_exit",
            a_crash_or_overflow_is_an_exit,
        ),
        (
            "cpu_and_wall_clock_limits_stop_a_loop",
            cpu_and_wall_clock_limits_stop_a_loop,
        ),
        (
            "the_memory_limit_contains_a_large_allocation",
            the_memory_limit_contains_a_large_allocation,
        ),
        (
            "frames_are_bounded_on_both_sides",
            frames_are_bounded_on_both_sides,
        ),
        (
            "a_finished_child_is_killed_and_reaped",
            a_finished_child_is_killed_and_reaped,
        ),
    ];
    for (name, check) in checks {
        check();
        println!("test {name} ... ok");
    }
    println!("test result: ok. {} passed", checks.len());
}

fn a_request_round_trips() {
    let (message, payload) = run("echo", &spec()).unwrap();
    assert_eq!(message.value, "request");
    assert_eq!(payload, b"payload");
}

fn extra_environment_reaches_the_child() {
    let (message, _) = run("environment", &spec()).unwrap();
    assert_eq!(message.value, "configured|2");
}

fn an_unregistered_service_is_unavailable() {
    assert!(matches!(
        run("missing", &spec()),
        Err(SandboxFailure::Unavailable(_))
    ));
    assert!((64..=113).contains(&UNKNOWN_SERVICE_EXIT_CODE));
    assert!(matches!(
        process_sandbox::run_program::<_, Message>(
            std::path::Path::new("/nonexistent/sandbox-host"),
            "echo",
            &Message {
                value: String::new()
            },
            &[],
            &spec(),
        ),
        Err(SandboxFailure::Unavailable(_))
    ));
}

fn a_crash_or_overflow_is_an_exit() {
    for service in ["abort", "overflow"] {
        let result = run(service, &spec());
        assert!(
            matches!(result, Err(SandboxFailure::Exited(Some(ref status))) if !status.success()),
            "{service}: {result:?}"
        );
    }
    // The parent is unaffected and the next request succeeds.
    assert!(run("echo", &spec()).is_ok());
}

fn cpu_and_wall_clock_limits_stop_a_loop() {
    let started = Instant::now();
    let wall = SandboxSpec {
        wall_clock: Duration::from_secs(1),
        ..spec()
    };
    assert_eq!(run("spin", &wall), Err(SandboxFailure::Timeout));
    assert!(started.elapsed() < Duration::from_secs(5));
    if cfg!(unix) {
        let started = Instant::now();
        let cpu = SandboxSpec {
            cpu_seconds: 1,
            wall_clock: Duration::from_secs(20),
            ..spec()
        };
        let result = run("spin", &cpu);
        assert!(
            matches!(result, Err(SandboxFailure::Exited(Some(_)))),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

fn the_memory_limit_contains_a_large_allocation() {
    if !MEMORY_LIMIT_ENFORCED {
        return;
    }
    let result = run("allocate", &spec());
    assert!(
        matches!(result, Err(SandboxFailure::Exited(_))),
        "{result:?}"
    );
}

fn frames_are_bounded_on_both_sides() {
    let oversized_request = process_sandbox::run::<_, Message>(
        "echo",
        &Message {
            value: String::new(),
        },
        &vec![0_u8; FRAMES.payload_bytes + 1],
        &spec(),
    );
    assert!(matches!(
        oversized_request,
        Err(SandboxFailure::Protocol(_))
    ));
    assert!(matches!(
        run("oversized", &spec()),
        Err(SandboxFailure::Protocol(_))
    ));
}

fn a_finished_child_is_killed_and_reaped() {
    let started = Instant::now();
    let (message, _) = run("answer_then_spin", &spec()).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    #[cfg(unix)]
    {
        let pid: libc::pid_t = message.value.parse().unwrap();
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "child {pid} is still running or unreaped");
    }
    #[cfg(not(unix))]
    let _ = message;
}
