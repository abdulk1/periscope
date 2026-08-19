//! Discovery, CRDs, generic tables and the detail view against a real cluster.

use std::time::{Duration, Instant};

use periscope_bridge::{ClusterCommand, ClusterEvent, KindId};
use periscope_e2e::{connected, context, describe, pods, wait_for, watching};
use periscope_store::AppState;

const TIMEOUT: Duration = Duration::from_secs(30);

#[test]
#[ignore = "needs a cluster"]
fn discovery_lists_built_in_kinds_and_custom_resources() {
    let (_runtime, stream, _cluster) = connected();

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("no discovery result; saw: {}", describe(&seen)));

    let ClusterEvent::Kinds { kinds, .. } = event else {
        unreachable!()
    };

    let labels: Vec<String> = kinds.iter().map(|info| info.id.label()).collect();
    for expected in ["pods", "deployments.apps", "nodes", "configmaps", "events"] {
        assert!(labels.contains(&expected.to_owned()), "missing {expected}");
    }

    // The CRD installed by the test fixture, discovered without special-casing.
    if !periscope_e2e::require(
        labels.contains(&"widgets.example.com".to_owned()),
        "widgets",
        "kubectl apply -f tests/e2e/fixtures/widgets.yaml",
    ) {
        return;
    }
    let widget = kinds
        .iter()
        .find(|info| info.id.label() == "widgets.example.com")
        .unwrap();
    assert!(widget.custom, "a CRD is a custom resource");
    assert!(widget.watchable);

    // Cluster-scoped kinds are marked as such, or the namespace filter would
    // build a URL that does not exist.
    let nodes = kinds
        .iter()
        .find(|info| info.id.label() == "nodes")
        .unwrap();
    assert!(!nodes.namespaced);
}

/// The Phase 2 acceptance criterion, spelled out: a cluster with Argo CD and
/// cert-manager installed lists their kinds without a line of code that knows
/// what Argo CD or cert-manager are.
#[test]
#[ignore = "needs a cluster with cert-manager and Argo CD installed"]
fn a_crd_heavy_cluster_lists_every_custom_resource() {
    if !periscope_e2e::require(
        periscope_e2e::serves_kind("certificates.cert-manager.io")
            && periscope_e2e::serves_kind("applications.argoproj.io"),
        "cert-manager and Argo CD",
        "see tests/e2e/README.md",
    ) {
        return;
    }

    let (_runtime, stream, _cluster) = connected();

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::Kinds { .. })
    })
    .unwrap_or_else(|seen| panic!("no discovery result; saw: {}", describe(&seen)));

    let ClusterEvent::Kinds { kinds, .. } = event else {
        unreachable!()
    };
    let labels: Vec<String> = kinds.iter().map(|info| info.id.label()).collect();

    for expected in [
        "applications.argoproj.io",
        "applicationsets.argoproj.io",
        "appprojects.argoproj.io",
        "certificates.cert-manager.io",
        "certificaterequests.cert-manager.io",
        "clusterissuers.cert-manager.io",
        "issuers.cert-manager.io",
        "orders.acme.cert-manager.io",
        "challenges.acme.cert-manager.io",
    ] {
        assert!(
            labels.contains(&expected.to_owned()),
            "missing {expected}; discovered {} kinds",
            labels.len()
        );
    }

    // All of them marked custom, and cluster-scoped ones marked as such.
    let issuers = kinds
        .iter()
        .find(|info| info.id.label() == "clusterissuers.cert-manager.io")
        .unwrap();
    assert!(issuers.custom);
    assert!(!issuers.namespaced, "ClusterIssuers are cluster-scoped");
}

#[test]
#[ignore = "needs a cluster"]
fn a_custom_resource_lists_through_the_same_path_as_pods() {
    if !periscope_e2e::require(
        periscope_e2e::serves_kind("widgets.example.com"),
        "widgets",
        "kubectl apply -f tests/e2e/fixtures/widgets.yaml",
    ) {
        return;
    }

    // The CRD is the fixture; the object is this test's, because an instance
    // cannot be applied in the same pass that creates its type.
    assert!(
        periscope_e2e::kubectl_apply(
            "apiVersion: example.com/v1\nkind: Widget\n\
             metadata: { name: sprocket, namespace: default }\n\
             spec: { size: 3 }\n"
        ),
        "could not create the widget this test lists"
    );

    let widgets = KindId::new("example.com", "v1", "Widget", "widgets");
    let (_runtime, stream, cluster) = watching(widgets.clone(), TIMEOUT);

    let (event, _) = wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind, .. } if kind == &widgets),
    )
    .unwrap_or_else(|seen| panic!("no widget listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, columns, .. } = &event else {
        unreachable!()
    };
    assert!(!rows.is_empty(), "the fixture widget should be listed");
    assert_eq!(rows[0].cells.len(), columns.len());

    let mut state = AppState::new();
    state.select_cluster(cluster);
    state.select_kind(widgets);
    state.apply_batch(std::slice::from_ref(&event), Instant::now());
    assert_eq!(&*state.rows()[0].key.name, "sprocket");
}

/// The headings a kind's table arrives with.
fn headings(event: &ClusterEvent) -> Vec<String> {
    let ClusterEvent::ResourceReset { columns, .. } = event else {
        panic!("expected a listing, got {event:?}")
    };
    columns.iter().map(|c| c.name.to_string()).collect()
}

/// Watches one kind and returns the listing the apiserver answered with.
fn listing(kind: KindId) -> ClusterEvent {
    let (_runtime, stream, _cluster) = watching(kind.clone(), TIMEOUT);

    wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind: listed, .. } if listed == &kind),
    )
    .unwrap_or_else(|seen| panic!("no {kind} listing; saw: {}", describe(&seen)))
    .0
}

/// cert-manager's Certificate CRD declares five printer columns: READY and
/// SECRET, ISSUER and STATUS at priority 1, and an AGE of its own. `kubectl get
/// certificates` prints NAME READY SECRET AGE, and so does this — the two
/// low-priority columns wait for a wide listing, and the CRD's AGE is the same
/// field as the AGE every table already ends with.
#[test]
#[ignore = "needs a cluster with cert-manager installed"]
fn a_crd_renders_the_printer_columns_its_author_declared() {
    let certificates = KindId::new("cert-manager.io", "v1", "Certificate", "certificates");
    let event = listing(certificates);

    assert_eq!(
        headings(&event),
        ["READY", "SECRET"],
        "cert-manager's own columns should be read from its CRD"
    );

    // The columns are declared whether or not anything has been issued, so the
    // headings are the assertion that always holds. Where a certificate does
    // exist, its cells are the proof that the JSONPaths were evaluated and not
    // merely parsed: READY comes from a filter over `status.conditions`, which
    // is the expression form worth having a real cluster for.
    let ClusterEvent::ResourceReset { rows, .. } = &event else {
        unreachable!()
    };
    for row in rows.iter() {
        assert!(
            matches!(row.cell(0), "True" | "False" | "Unknown" | "<none>"),
            "{}: READY should be a condition status, got {:?}",
            row.key,
            row.cell(0)
        );
        assert!(
            !row.cell(1).is_empty(),
            "{}: SECRET should name a secret or say <none>",
            row.key
        );
    }
}

/// Argo CD's Application CRD, for a second vendor's declarations and a heading
/// with a space in it.
#[test]
#[ignore = "needs a cluster with Argo CD installed"]
fn a_second_vendors_crd_renders_its_columns_too() {
    let applications = KindId::new("argoproj.io", "v1alpha1", "Application", "applications");
    let headings = headings(&listing(applications));

    assert_eq!(headings, ["SYNC STATUS", "HEALTH STATUS"]);
    // REVISION and PROJECT are priority 10; `kubectl get applications` hides
    // both until asked for `-o wide`.
    assert!(!headings.contains(&"REVISION".to_owned()), "{headings:?}");
    assert!(!headings.contains(&"PROJECT".to_owned()), "{headings:?}");
}

/// A CRD may declare nothing, and most do. The generic table is still there for
/// them; it is simply no longer every custom resource's answer.
#[test]
#[ignore = "needs a cluster"]
fn a_crd_that_declares_no_printer_columns_falls_back_to_the_generic_table() {
    let widgets = KindId::new("example.com", "v1", "Widget", "widgets");
    let headings = headings(&listing(widgets));

    assert_eq!(headings, ["STATUS", "READY"]);
}

#[test]
#[ignore = "needs a cluster"]
fn deployments_render_the_columns_kubectl_prints() {
    let deployments = KindId::new("apps", "v1", "Deployment", "deployments");
    let (_runtime, stream, _cluster) = watching(deployments.clone(), TIMEOUT);

    let (event, _) = wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind, .. } if kind == &deployments),
    )
    .unwrap_or_else(|seen| panic!("no deployment listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { columns, rows, .. } = event else {
        unreachable!()
    };
    let headings: Vec<String> = columns.iter().map(|c| c.name.to_string()).collect();
    assert_eq!(headings, ["READY", "UP-TO-DATE", "AVAILABLE"]);
    // kind runs CoreDNS, so there is always at least one deployment.
    assert!(!rows.is_empty());
}

#[test]
#[ignore = "needs a cluster"]
fn fetching_an_object_returns_yaml_owners_and_events() {
    let (runtime, stream, cluster) = watching(pods(), TIMEOUT);

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::ResourceReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, .. } = event else {
        unreachable!()
    };
    // A kube-system pod: owned by something, and old enough to have events.
    let row = rows
        .iter()
        .find(|row| &*row.key.namespace == "kube-system")
        .expect("kube-system pods exist");

    runtime
        .send(ClusterCommand::FetchObject {
            cluster,
            kind: pods(),
            key: row.key.clone(),
            reveal: false,
        })
        .expect("fetch is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Object { .. } | ClusterEvent::ObjectFailed { .. }
        )
    })
    .unwrap_or_else(|seen| panic!("no object came back; saw: {}", describe(&seen)));

    let ClusterEvent::Object { detail, .. } = event else {
        panic!("the fetch failed: {event:?}")
    };

    assert!(detail.yaml.contains("kind: Pod"), "{}", detail.yaml);
    assert!(detail.yaml.contains(&*row.key.name));
    // The noise kubectl hides is hidden here too.
    assert!(!detail.yaml.contains("managedFields"), "{}", detail.yaml);
    assert!(!detail.yaml.contains("resourceVersion"), "{}", detail.yaml);
}

#[test]
#[ignore = "needs a cluster"]
fn a_namespace_filter_narrows_the_listing_server_side() {
    let (runtime, stream, cluster) = watching(pods(), TIMEOUT);
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::ResourceReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing; saw: {}", describe(&seen)));

    runtime
        .send(ClusterCommand::Watch {
            cluster,
            kind: pods(),
            namespace: Some(std::sync::Arc::from("kube-system")),
            selector: None,
        })
        .expect("watch is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::ResourceReset { rows, .. }
            if rows.iter().all(|row| &*row.key.namespace == "kube-system") && !rows.is_empty())
    })
    .unwrap_or_else(|seen| panic!("no narrowed listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, .. } = event else {
        unreachable!()
    };
    assert!(rows.iter().all(|row| &*row.key.namespace == "kube-system"));
}

#[test]
#[ignore = "needs a cluster"]
fn a_label_selector_is_applied_by_the_apiserver() {
    let (runtime, stream, cluster) = watching(pods(), TIMEOUT);
    wait_for(&stream, TIMEOUT, |event| {
        matches!(event, ClusterEvent::ResourceReset { .. })
    })
    .unwrap_or_else(|seen| panic!("no pod listing; saw: {}", describe(&seen)));

    runtime
        .send(ClusterCommand::Watch {
            cluster,
            kind: pods(),
            namespace: None,
            selector: Some(std::sync::Arc::from("k8s-app=kube-dns")),
        })
        .expect("watch is queued");

    let (event, _) = wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { rows, .. } if !rows.is_empty()),
    )
    .unwrap_or_else(|seen| panic!("no selected listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, .. } = event else {
        unreachable!()
    };
    assert!(
        rows.iter().all(|row| row.key.name.starts_with("coredns")),
        "the selector should have narrowed to CoreDNS: {:?}",
        rows.iter().map(|r| r.key.to_string()).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "needs a cluster"]
fn nodes_are_cluster_scoped_and_render_their_roles() {
    let nodes = KindId::new("", "v1", "Node", "nodes");
    let (_runtime, stream, _cluster) = watching(nodes.clone(), TIMEOUT);

    let (event, _) = wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind, .. } if kind == &nodes),
    )
    .unwrap_or_else(|seen| panic!("no node listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, columns, .. } = event else {
        unreachable!()
    };
    assert!(!rows.is_empty());
    assert!(!rows[0].key.is_namespaced(), "nodes are cluster-scoped");

    let headings: Vec<String> = columns.iter().map(|c| c.name.to_string()).collect();
    assert_eq!(headings, ["STATUS", "ROLES", "VERSION"]);
    assert_eq!(&*rows[0].cells[0], "Ready");
    assert!(rows[0].cells[1].contains("control-plane"));
}

#[test]
#[ignore = "needs a cluster"]
fn a_kind_that_does_not_exist_degrades_rather_than_hanging() {
    let ghost = KindId::new("example.com", "v1", "Ghost", "ghosts");
    let (_runtime, stream, _cluster) = watching(ghost, TIMEOUT);

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Status {
                state: periscope_bridge::ConnectionState::Degraded { .. },
                ..
            }
        )
    })
    .unwrap_or_else(|seen| panic!("no failure reported; saw: {}", describe(&seen)));

    let ClusterEvent::Status { state, .. } = event else {
        unreachable!()
    };
    let reason = state.detail().unwrap();
    assert!(
        reason.starts_with("ghosts.example.com:"),
        "the failing kind should be named: {reason}"
    );
}

/// The Phase 2 budget says a large ConfigMap's YAML opens without a visible
/// frame drop. What is measured here is everything up to the pixels: the fetch,
/// the noise removal and the YAML rendering. The frame it lands in is not
/// measured — see `docs/LIMITATIONS.md`.
#[test]
#[ignore = "needs a cluster with the large ConfigMap fixture"]
fn a_large_config_map_renders_to_yaml_well_inside_a_frame() {
    if !periscope_e2e::require(
        periscope_e2e::object_exists("configmap", "default", "periscope-large"),
        "the large ConfigMap",
        "cargo run -p periscope-e2e --bin seed-pods -- --large-config-map",
    ) {
        return;
    }

    let config_maps = KindId::new("", "v1", "ConfigMap", "configmaps");
    let (runtime, stream, cluster) = watching(config_maps.clone(), TIMEOUT);

    wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind, .. } if kind == &config_maps),
    )
    .unwrap_or_else(|seen| panic!("no config map listing; saw: {}", describe(&seen)));

    let started = Instant::now();
    runtime
        .send(ClusterCommand::FetchObject {
            cluster,
            kind: config_maps,
            key: periscope_bridge::ResourceKey::new("default", "periscope-large"),
            reveal: false,
        })
        .expect("fetch is queued");

    let (event, _) = wait_for(&stream, TIMEOUT, |event| {
        matches!(
            event,
            ClusterEvent::Object { .. } | ClusterEvent::ObjectFailed { .. }
        )
    })
    .unwrap_or_else(|seen| panic!("no object came back; saw: {}", describe(&seen)));
    let elapsed = started.elapsed();

    let ClusterEvent::Object { detail, .. } = event else {
        panic!("the fetch failed — seed the fixture first: {event:?}")
    };

    println!(
        "{} KiB of YAML fetched and rendered in {elapsed:?}",
        detail.yaml.len() / 1024
    );
    assert!(
        detail.yaml.len() > 900_000,
        "the fixture should be about a megabyte, was {}",
        detail.yaml.len()
    );
    // A whole megabyte over the wire, deserialised, cleaned and rendered: it is
    // a network round trip that dominates, not the rendering.
    assert!(
        elapsed < Duration::from_millis(500),
        "fetching and rendering took {elapsed:?}"
    );
}

#[test]
#[ignore = "needs a cluster"]
fn a_secret_is_masked_until_it_is_revealed() {
    let secrets = KindId::new("", "v1", "Secret", "secrets");
    let (runtime, stream, cluster) = watching(secrets.clone(), TIMEOUT);

    let (event, _) = wait_for(
        &stream,
        TIMEOUT,
        |event| matches!(event, ClusterEvent::ResourceReset { kind, .. } if kind == &secrets),
    )
    .unwrap_or_else(|seen| panic!("no secret listing; saw: {}", describe(&seen)));

    let ClusterEvent::ResourceReset { rows, .. } = event else {
        unreachable!()
    };
    let row = rows
        .iter()
        .find(|row| !row.cells.is_empty() && row.cell(1) != "0")
        .expect("some secret has data");

    // The table never carries a value, only the key count.
    assert!(
        !row.cells.iter().any(|cell| cell.len() > 40),
        "a table cell looks like it holds a secret value: {:?}",
        row.cells
    );

    for reveal in [false, true] {
        runtime
            .send(ClusterCommand::FetchObject {
                cluster: cluster.clone(),
                kind: secrets.clone(),
                key: row.key.clone(),
                reveal,
            })
            .expect("fetch is queued");

        let (event, _) = wait_for(&stream, TIMEOUT, |event| {
            matches!(
                event,
                ClusterEvent::Object { .. } | ClusterEvent::ObjectFailed { .. }
            )
        })
        .unwrap_or_else(|seen| panic!("no object came back; saw: {}", describe(&seen)));

        let ClusterEvent::Object { detail, .. } = event else {
            panic!("the fetch failed: {event:?}")
        };

        assert!(detail.maskable, "a Secret is maskable");
        assert_eq!(detail.revealed, reveal);
        if reveal {
            assert!(!detail.yaml.contains("<hidden"), "{}", detail.yaml);
        } else {
            assert!(
                detail.yaml.contains("<hidden"),
                "secret values must be masked by default: {}",
                detail.yaml
            );
        }
    }
}

#[test]
#[ignore = "needs a cluster"]
fn the_context_is_the_one_the_tests_target() {
    assert_eq!(context().as_str(), "kind-periscope");
}
