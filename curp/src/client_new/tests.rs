use std::{
    collections::HashMap,
    ops::AddAssign,
    sync::{Arc, Mutex},
};

use curp_test_utils::test_cmd::{TestCommand, TestCommandResult};
use dashmap::DashMap;
use tracing_test::traced_test;

use super::unary::Unary;
use crate::{
    client_new::ClientApi,
    members::ServerId,
    rpc::{
        connect::{ConnectApi, MockConnectApi},
        CurpError, FetchClusterResponse, Member, ProposeId, ProposeResponse,
    },
};

/// Create a mocked connects with server id from 0~size
#[allow(trivial_casts)] // Trait object with high ranked type inferences failed, cast manually
fn init_mocked_connects(
    size: usize,
    f: impl Fn(usize, &mut MockConnectApi),
) -> DashMap<ServerId, Arc<dyn ConnectApi>> {
    std::iter::repeat_with(|| MockConnectApi::new())
        .take(size)
        .enumerate()
        .map(|(id, mut conn)| {
            conn.expect_id().returning(move || id as ServerId);
            conn.expect_update_addrs().returning(|_addr| Ok(()));
            f(id, &mut conn);
            (id as ServerId, Arc::new(conn) as Arc<dyn ConnectApi>)
        })
        .collect()
}

// Tests for unary client

#[traced_test]
#[tokio::test]
async fn test_unary_fetch_clusters_serializable() {
    let connects = init_mocked_connects(3, |_id, conn| {
        conn.expect_fetch_cluster().return_once(|_req, _timeout| {
            Ok(tonic::Response::new(FetchClusterResponse {
                leader_id: Some(0),
                term: 1,
                cluster_id: 123,
                members: vec![
                    Member::new(0, "S0", vec!["A0".to_owned()], false),
                    Member::new(1, "S1", vec!["A1".to_owned()], false),
                    Member::new(2, "S2", vec!["A2".to_owned()], false),
                ],
                cluster_version: 1,
            }))
        });
    });
    let unary = Unary::<TestCommand>::new(connects, None, None);
    assert!(unary.local_connect().is_none());
    let res = unary.fetch_cluster(false).await.unwrap();
    assert_eq!(
        res.into_members_addrs(),
        HashMap::from([
            (0, vec!["A0".to_owned()]),
            (1, vec!["A1".to_owned()]),
            (2, vec!["A2".to_owned()])
        ])
    );
}

#[traced_test]
#[tokio::test]
async fn test_unary_fetch_clusters_serializable_local_first() {
    let connects = init_mocked_connects(3, |id, conn| {
        conn.expect_fetch_cluster()
            .return_once(move |_req, _timeout| {
                let members = if id == 1 {
                    // local server(1) does not see the cluster members
                    vec![]
                } else {
                    panic!("other server's `fetch_cluster` should not be invoked");
                };
                Ok(tonic::Response::new(FetchClusterResponse {
                    leader_id: Some(0),
                    term: 1,
                    cluster_id: 123,
                    members,
                    cluster_version: 1,
                }))
            });
    });
    let unary = Unary::<TestCommand>::new(connects, Some(1), None);
    assert!(unary.local_connect().is_some());
    let res = unary.fetch_cluster(false).await.unwrap();
    assert!(res.members.is_empty());
}

#[traced_test]
#[tokio::test]
async fn test_unary_fetch_clusters_linearizable() {
    let connects = init_mocked_connects(5, |id, conn| {
        conn.expect_fetch_cluster()
            .return_once(move |_req, _timeout| {
                let resp = match id {
                    0 => FetchClusterResponse {
                        leader_id: Some(0),
                        term: 2,
                        cluster_id: 123,
                        members: vec![
                            Member::new(0, "S0", vec!["A0".to_owned()], false),
                            Member::new(1, "S1", vec!["A1".to_owned()], false),
                            Member::new(2, "S2", vec!["A2".to_owned()], false),
                            Member::new(3, "S3", vec!["A3".to_owned()], false),
                            Member::new(4, "S4", vec!["A4".to_owned()], false),
                        ],
                        cluster_version: 1,
                    },
                    1 | 4 => FetchClusterResponse {
                        leader_id: Some(0),
                        term: 2,
                        cluster_id: 123,
                        members: vec![], // linearizable read from follower returns empty members
                        cluster_version: 1,
                    },
                    2 => FetchClusterResponse {
                        leader_id: None, // imagine this node is a disconnected candidate
                        term: 23,        // with a high term
                        cluster_id: 123,
                        members: vec![],
                        cluster_version: 1,
                    },
                    3 => FetchClusterResponse {
                        leader_id: Some(3), // imagine this node is a old leader
                        term: 1,            // with the old term
                        cluster_id: 123,
                        members: vec![
                            Member::new(0, "S0", vec!["B0".to_owned()], false),
                            Member::new(1, "S1", vec!["B1".to_owned()], false),
                            Member::new(2, "S2", vec!["B2".to_owned()], false),
                            Member::new(3, "S3", vec!["B3".to_owned()], false),
                            Member::new(4, "S4", vec!["B4".to_owned()], false),
                        ],
                        cluster_version: 1,
                    },
                    _ => unreachable!("there are only 5 nodes"),
                };
                Ok(tonic::Response::new(resp))
            });
    });
    let unary = Unary::<TestCommand>::new(connects, None, None);
    assert!(unary.local_connect().is_none());
    let res = unary.fetch_cluster(true).await.unwrap();
    assert_eq!(
        res.into_members_addrs(),
        HashMap::from([
            (0, vec!["A0".to_owned()]),
            (1, vec!["A1".to_owned()]),
            (2, vec!["A2".to_owned()]),
            (3, vec!["A3".to_owned()]),
            (4, vec!["A4".to_owned()])
        ])
    );
}

#[traced_test]
#[tokio::test]
async fn test_unary_fetch_clusters_linearizable_failed() {
    let connects = init_mocked_connects(5, |id, conn| {
        conn.expect_fetch_cluster()
            .return_once(move |_req, _timeout| {
                let resp = match id {
                    0 => FetchClusterResponse {
                        leader_id: Some(0),
                        term: 2,
                        cluster_id: 123,
                        members: vec![
                            Member::new(0, "S0", vec!["A0".to_owned()], false),
                            Member::new(1, "S1", vec!["A1".to_owned()], false),
                            Member::new(2, "S2", vec!["A2".to_owned()], false),
                            Member::new(3, "S3", vec!["A3".to_owned()], false),
                            Member::new(4, "S4", vec!["A4".to_owned()], false),
                        ],
                        cluster_version: 1,
                    },
                    1 => FetchClusterResponse {
                        leader_id: Some(0),
                        term: 2,
                        cluster_id: 123,
                        members: vec![], // linearizable read from follower returns empty members
                        cluster_version: 1,
                    },
                    2 => FetchClusterResponse {
                        leader_id: None, // imagine this node is a disconnected candidate
                        term: 23,        // with a high term
                        cluster_id: 123,
                        members: vec![],
                        cluster_version: 1,
                    },
                    3 => FetchClusterResponse {
                        leader_id: Some(3), // imagine this node is a old leader
                        term: 1,            // with the old term
                        cluster_id: 123,
                        members: vec![
                            Member::new(0, "S0", vec!["B0".to_owned()], false),
                            Member::new(1, "S1", vec!["B1".to_owned()], false),
                            Member::new(2, "S2", vec!["B2".to_owned()], false),
                            Member::new(3, "S3", vec!["B3".to_owned()], false),
                            Member::new(4, "S4", vec!["B4".to_owned()], false),
                        ],
                        cluster_version: 1,
                    },
                    4 => FetchClusterResponse {
                        leader_id: Some(3), // imagine this node is a old follower of old leader(3)
                        term: 1,            // with the old term
                        cluster_id: 123,
                        members: vec![],
                        cluster_version: 1,
                    },
                    _ => unreachable!("there are only 5 nodes"),
                };
                Ok(tonic::Response::new(resp))
            });
    });
    let unary = Unary::<TestCommand>::new(connects, None, None);
    assert!(unary.local_connect().is_none());
    let res = unary.fetch_cluster(true).await.unwrap_err();
    // only server(0, 1)'s responses are valid, less than majority quorum(3), got a mocked RpcTransport to retry
    assert_eq!(res, CurpError::RpcTransport(()));
}

#[traced_test]
#[tokio::test]
async fn test_unary_fast_round_works() {
    let connects = init_mocked_connects(5, |id, conn| {
        conn.expect_propose().return_once(move |_req, _timeout| {
            let resp = match id {
                0 => ProposeResponse::new_result::<TestCommand>(&Ok(TestCommandResult::default())),
                1 | 2 | 3 => ProposeResponse::new_empty(),
                4 => return Err(CurpError::key_conflict()),
                _ => unreachable!("there are only 5 nodes"),
            };
            Ok(tonic::Response::new(resp))
        });
    });
    let unary = Unary::<TestCommand>::new(connects, None, None);
    let res = unary
        .fast_round(ProposeId(0, 0), &TestCommand::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res, TestCommandResult::default());
}

#[traced_test]
#[tokio::test]
async fn test_unary_fast_round_return_early_err() {
    for early_err in [
        CurpError::duplicated(),
        CurpError::shutting_down(),
        CurpError::invalid_config(),
        CurpError::node_already_exists(),
        CurpError::node_not_exist(),
        CurpError::learner_not_catch_up(),
        CurpError::expired_client_id(),
        CurpError::wrong_cluster_version(),
        CurpError::redirect(Some(1), 0),
    ] {
        assert!(early_err.return_early());
        // record how many times `handle_propose` was invoked.
        let counter = Arc::new(Mutex::new(0));
        let connects = init_mocked_connects(3, |_id, conn| {
            let counter_c = Arc::clone(&counter);
            let err = early_err.clone();
            conn.expect_propose().return_once(move |_req, _timeout| {
                counter_c.lock().unwrap().add_assign(1);
                Err(err)
            });
        });
        let unary = Unary::<TestCommand>::new(connects, None, None);
        let err = unary
            .fast_round(ProposeId(0, 0), &TestCommand::default())
            .await
            .unwrap_err();
        assert_eq!(err, early_err);
        assert_eq!(*counter.lock().unwrap(), 1);
    }
}

#[traced_test]
#[tokio::test]
async fn test_unary_fast_round_less_quorum() {
    let connects = init_mocked_connects(5, |id, conn| {
        conn.expect_propose().return_once(move |_req, _timeout| {
            let resp = match id {
                0 => ProposeResponse::new_result::<TestCommand>(&Ok(TestCommandResult::default())),
                1 | 2 => ProposeResponse::new_empty(),
                3 | 4 => return Err(CurpError::key_conflict()),
                _ => unreachable!("there are only 5 nodes"),
            };
            Ok(tonic::Response::new(resp))
        });
    });
    let unary = Unary::<TestCommand>::new(connects, None, None);
    let err = unary
        .fast_round(ProposeId(0, 0), &TestCommand::default())
        .await
        .unwrap_err();
    assert_eq!(err, CurpError::KeyConflict(()));
}

/// FIXME: two leader
/// TODO: fix in subsequence PR
#[traced_test]
#[tokio::test]
#[should_panic]
async fn test_unary_fast_round_with_two_leader() {
    let connects = init_mocked_connects(5, |id, conn| {
        conn.expect_propose().return_once(move |_req, _timeout| {
            let resp = match id {
                // The execution result has been returned, indicating that server(0) has also recorded the command.
                0 => ProposeResponse::new_result::<TestCommand>(&Ok(TestCommandResult::new(
                    vec![1],
                    vec![1],
                ))),
                // imagine that server(1) is the new leader
                1 => ProposeResponse::new_result::<TestCommand>(&Ok(TestCommandResult::new(
                    vec![2],
                    vec![2],
                ))),
                2 | 3 => ProposeResponse::new_empty(),
                4 => return Err(CurpError::key_conflict()),
                _ => unreachable!("there are only 5 nodes"),
            };
            Ok(tonic::Response::new(resp))
        });
    });
    // old local leader(0), term 1
    let unary = Unary::<TestCommand>::new(connects, None, Some((0, 1)));
    let res = unary
        .fast_round(ProposeId(0, 0), &TestCommand::default())
        .await
        .unwrap()
        .unwrap();
    // quorum: server(0, 1, 2, 3)
    assert_eq!(res, TestCommandResult::new(vec![2], vec![2]));
}
