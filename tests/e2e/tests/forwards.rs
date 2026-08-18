//! Port forwarding against a real cluster.
//!
//! Needs the `webby` fixture — one pod serving a fixed string on port 8080:
//!
//! ```text
//! kubectl apply -f tests/e2e/fixtures/webby.yaml
//! ```

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use periscope_bridge::{
    ClusterCommand, ClusterEvent, ClusterId, ClusterRuntime, EventStream, ForwardId, ForwardState,
    ForwardTarget, RuntimeConfig,
};
use periscope_cluster::KubeHandler;
use periscope_e2e::{context, describe, wait_for};

const TIMEOUT: Duration = Duration::from_secs(30);

/// The label the fixture's pod carries.
const FIXTURE: &str = "app=webby";

/// What the fixture serves.
const BODY: &str = "periscope-forward-ok";

fn connected() -> (ClusterRuntime, EventStream, ClusterId) {
    let (runtime, stream) = ClusterRuntime::start(KubeHandler::new(), RuntimeConfig::default())
        .expect("the runtime starts");
    let cluster = context();

    runtime
        .send(ClusterCommand::Connect {
            cluster: cluster.clone(),
        })
        .expect("connect is queued");
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("discovery never finished; saw: {}", describe(&seen)));

    (runtime, stream, cluster)
}

/// Starts a forward and waits until it is listening, returning the local port.
fn listening(
    runtime: &ClusterRuntime,
    stream: &EventStream,
    cluster: &ClusterId,
    id: ForwardId,
    target: ForwardTarget,
) -> u16 {
    runtime
        .send(ClusterCommand::StartForward {
            cluster: cluster.clone(),
            id,
            target: Arc::new(target),
        })
        .expect("the command is queued");

    let (event, _) = wait_for(stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::ForwardChanged { forward, .. }
                if matches!(forward.state, ForwardState::Listening { .. } | ForwardState::Failed { .. })
        )
    })
    .unwrap_or_else(|seen| panic!("the forward never came up; saw: {}", describe(&seen)));

    let ClusterEvent::ForwardChanged { forward, .. } = event else {
        unreachable!()
    };
    match &forward.state {
        ForwardState::Listening { local_port } => *local_port,
        other => panic!("the forward failed: {other:?}"),
    }
}

/// One HTTP GET through a local port.
fn get(port: u16) -> std::io::Result<String> {
    let mut socket = TcpStream::connect(("127.0.0.1", port))?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;

    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    Ok(response)
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_forward_carries_real_traffic() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(1),
        ForwardTarget::new("default", &pod, 8080),
    );

    let response = get(port).expect("the forwarded port answers");
    assert!(response.contains(BODY), "unexpected response: {response}");
    // busybox's httpd answers 1.1 whatever the request said; what matters is
    // that a real server answered through the tunnel.
    assert!(response.starts_with("HTTP/1."), "{response}");
    assert!(response.contains(" 200 "), "{response}");
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_forward_serves_more_than_one_connection() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(2),
        ForwardTarget::new("default", &pod, 8080),
    );

    // Each connection gets its own stream to the apiserver; the listener is
    // what persists.
    for attempt in 0..3 {
        let response = get(port).unwrap_or_else(|error| panic!("attempt {attempt}: {error}"));
        assert!(response.contains(BODY), "attempt {attempt}: {response}");
    }
}

#[test]
#[ignore = "needs the webby fixture"]
fn stopping_a_forward_closes_the_port() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(3),
        ForwardTarget::new("default", &pod, 8080),
    );
    assert!(get(port).is_ok(), "the forward should work before stopping");

    runtime
        .send(ClusterCommand::StopForward {
            cluster: cluster.clone(),
            id: ForwardId(3),
        })
        .expect("the command is queued");

    wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::ForwardChanged { forward, .. }
                if forward.state == ForwardState::Stopped
        )
    })
    .unwrap_or_else(|seen| panic!("no teardown was reported; saw: {}", describe(&seen)));

    // The listener is gone: the port refuses connections rather than accepting
    // them and going nowhere.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the port is still accepting connections after the forward stopped");
}

#[test]
#[ignore = "needs the webby fixture"]
fn forwarding_a_port_the_pod_does_not_serve_fails_loudly() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    // The listener binds — nothing is wrong locally — and the failure appears
    // when a connection is actually attempted.
    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(4),
        ForwardTarget::new("default", &pod, 9999),
    );

    let _ = get(port);

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::ForwardChanged { forward, .. } if forward.state.is_problem()
        )
    })
    .unwrap_or_else(|seen| {
        panic!(
            "a dead port must be reported, not silently accepted; saw: {}",
            describe(&seen)
        )
    });

    let ClusterEvent::ForwardChanged { forward, .. } = event else {
        unreachable!()
    };
    let reason = forward.state.detail().expect("a reason is carried");
    assert!(!reason.is_empty(), "the reason should say something");
    // Degraded, not dead: the listener is still bound, so it recovers by
    // itself if whatever should be listening starts.
    assert!(!forward.state.is_over(), "{:?}", forward.state);
}

#[test]
#[ignore = "needs the webby fixture"]
fn a_forward_recovers_after_a_failed_connection() {
    let Some(pod) = periscope_e2e::fixture("webby", FIXTURE) else {
        return;
    };
    let (runtime, stream, cluster) = connected();

    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(5),
        ForwardTarget::new("default", &pod, 8080),
    );

    // Half-open a connection and drop it: the stream breaks, the listener does
    // not.
    {
        let socket = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        drop(socket);
    }
    std::thread::sleep(Duration::from_millis(500));

    let response = get(port).expect("the forward still works after a broken connection");
    assert!(response.contains(BODY), "{response}");
}

#[test]
#[ignore = "needs a cluster"]
fn forwarding_to_a_pod_that_does_not_exist_says_so() {
    let (runtime, stream, cluster) = connected();

    let port = listening(
        &runtime,
        &stream,
        &cluster,
        ForwardId(6),
        ForwardTarget::new("default", "periscope-no-such-pod", 8080),
    );
    let _ = get(port);

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::ForwardChanged { forward, .. } if forward.state.is_problem()
        )
    })
    .unwrap_or_else(|seen| panic!("no failure was reported; saw: {}", describe(&seen)));

    let ClusterEvent::ForwardChanged { forward, .. } = event else {
        unreachable!()
    };
    let reason = forward.state.detail().expect("a reason is carried");
    assert!(
        reason.contains("periscope-no-such-pod") || reason.contains("not found"),
        "the reason should name what went wrong: {reason}"
    );
}
