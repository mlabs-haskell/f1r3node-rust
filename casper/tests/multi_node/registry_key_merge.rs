use casper::rust::block_status::ValidBlock;
use casper::rust::util::{construct_deploy, rspace_util};
use models::rust::casper::protocol::casper_message::BlockMessage;
use rspace_plus_plus::rspace::history::Either;

use crate::helper::test_node::TestNode;
use crate::util::genesis_builder::GenesisBuilder;

const INIT_MAP: &str = r#"
new rl(`rho:registry:lookup`), thmCh, mapCh, ack in {
  rl!(`rho:lang:treeHashMap`, *thmCh) |
  for (thm <- thmCh) {
    thm!("init", 1, *mapCh) |
    for (@map <- mapCh) {
      thm!("set", map, "k", 0, *ack) |
      for (_ <- ack) { @"registry-merge-map"!(map) }
    }
  }
}
"#;

// Same as INIT_MAP but seeds no key, so a later write to any key creates that
// key's leaf from scratch.
const INIT_MAP_EMPTY: &str = r#"
new rl(`rho:registry:lookup`), thmCh, mapCh in {
  rl!(`rho:lang:treeHashMap`, *thmCh) |
  for (thm <- thmCh) {
    thm!("init", 1, *mapCh) |
    for (@map <- mapCh) {
      @"registry-merge-map"!(map)
    }
  }
}
"#;

fn set_key(key: &str, value: i64) -> String {
    format!(
        r#"
new rl(`rho:registry:lookup`), thmCh, ack in {{
  rl!(`rho:lang:treeHashMap`, *thmCh) |
  for (thm <- thmCh) {{
    for (@map <<- @"registry-merge-map") {{
      thm!("set", map, "{key}", {value}, *ack)
    }}
  }}
}}
"#
    )
}

fn assert_no_failed_deploys(block: &BlockMessage) {
    assert!(
        block.body.deploys.iter().all(|deploy| !deploy.is_failed),
        "a deploy failed in block {}: {:?}",
        hex::encode(&block.block_hash[..block.block_hash.len().min(8)]),
        block
            .body
            .deploys
            .iter()
            .map(|deploy| deploy.system_deploy_error.clone())
            .collect::<Vec<_>>()
    );
}

/// Two sibling blocks write `key1` and `key2` into one TreeHashMap that already
/// holds `"k"`. Returns whether each write was rejected in the merge block,
/// plus the network and the merge block so callers can build on top of it.
async fn merge_two_writes(
    init: &str,
    key1: &str,
    key2: &str,
) -> (bool, bool, Vec<TestNode>, BlockMessage) {
    crate::init_logger();

    let genesis = GenesisBuilder::new()
        .build_genesis_with_parameters(None)
        .await
        .unwrap();
    let shard_id = genesis.genesis_block.shard_id.clone();
    let mut nodes = TestNode::create_network(genesis, 3, None, None, None, None)
        .await
        .unwrap();
    for node in nodes.iter_mut() {
        node.allow_empty_blocks = true;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let deploy = |source: String, sec, offset: i64| {
        construct_deploy::source_deploy(
            source,
            now + offset,
            Some(5_000_000),
            None,
            Some(sec),
            None,
            Some(shard_id.clone()),
        )
        .unwrap()
    };

    let base = nodes[0]
        .add_block_from_deploys(&[deploy(
            init.to_string(),
            construct_deploy::DEFAULT_SEC.clone(),
            0,
        )])
        .await
        .expect("validator 0 proposes the map");
    assert_no_failed_deploys(&base);
    TestNode::sync_all(&mut nodes).await.unwrap();

    let write1 = deploy(set_key(key1, 1), construct_deploy::DEFAULT_SEC.clone(), 1);
    let write2 = deploy(set_key(key2, 2), construct_deploy::DEFAULT_SEC2.clone(), 2);
    let nil_sibling = deploy("Nil".to_string(), construct_deploy::DEFAULT_SEC2.clone(), 3);
    let trigger = deploy("Nil".to_string(), construct_deploy::DEFAULT_SEC.clone(), 4);

    let block1 = nodes[0]
        .add_block_from_deploys(std::slice::from_ref(&write1))
        .await
        .expect("validator 0 writes");
    let block2 = nodes[1]
        .add_block_from_deploys(std::slice::from_ref(&write2))
        .await
        .expect("validator 1 writes");
    nodes[2]
        .add_block_from_deploys(std::slice::from_ref(&nil_sibling))
        .await
        .expect("validator 2 proposes a sibling");
    assert_no_failed_deploys(&block1);
    assert_no_failed_deploys(&block2);
    TestNode::sync_all(&mut nodes).await.unwrap();

    let merge_block = nodes[0]
        .add_block_from_deploys(std::slice::from_ref(&trigger))
        .await
        .expect("validator 0 proposes the merge block");
    assert!(merge_block.header.parents_hash_list.len() >= 2);

    for node in nodes[1..].iter_mut() {
        let status = node.process_block(merge_block.clone()).await.unwrap();
        assert_eq!(status, Either::Right(ValidBlock::Valid));
    }

    let rejected: Vec<_> = merge_block
        .body
        .rejected_deploys
        .iter()
        .map(|rejected| rejected.sig.clone())
        .collect();
    (
        rejected.contains(&write1.sig),
        rejected.contains(&write2.sig),
        nodes,
        merge_block,
    )
}

/// Both writes consume the same leaf datum, so the merge keeps exactly one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_the_same_registry_key_reject_exactly_one() {
    let (write1_rejected, write2_rejected, ..) = merge_two_writes(INIT_MAP, "k", "k").await;
    assert!(
        write1_rejected != write2_rejected,
        "write1_rejected={write1_rejected}, write2_rejected={write2_rejected}"
    );
}

/// New keys in different leaves only set bits in the shared interior bitmap,
/// which BitmaskOr merges, so neither write is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_different_registry_keys_reject_none() {
    let (write1_rejected, write2_rejected, ..) = merge_two_writes(INIT_MAP, "a", "b").await;
    assert!(
        !write1_rejected && !write2_rejected,
        "write1_rejected={write1_rejected}, write2_rejected={write2_rejected}"
    );
}

/// Two sibling blocks write the SAME unseeded key. Each deploy creates the
/// key's leaf from scratch (MakeNode produces 0, the setter consumes it and
/// produces its own Map), so the two writes share no destroyed event and the
/// merge conflict check never fires. The leaf Map is non-numeric, so the
/// single-value overfill guard exempts it too. Expected (correct) behavior is
/// exactly one rejection, as in the seeded same-key case; the suspected bug is
/// that neither is rejected and the leaf ends up holding two datums.
///
/// A probe block on top of the merge consumes the key's leaf twice in sequence
/// with the test-only `unsafeConsume`, publishing each value on `@11` and
/// `@12`. A single leaf datum lets only the first consume fire, so `@12` stays
/// empty; a second datum shows up as a value on `@12`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_to_the_same_fresh_registry_key_leave_one_leaf_datum() {
    let (write1_rejected, write2_rejected, mut nodes, merge_block) =
        merge_two_writes(INIT_MAP_EMPTY, "a", "a").await;

    let probe = construct_deploy::source_deploy(
        r#"
new rl(`rho:registry:lookup`), thmCh, ret1, ret2 in {
  rl!(`rho:lang:treeHashMap`, *thmCh) |
  for (thm <- thmCh) {
    for (@map <<- @"registry-merge-map") {
      thm!("unsafeConsume", map, "a", *ret1) |
      for (@v1 <- ret1) {
        @11!(v1) |
        thm!("unsafeConsume", map, "a", *ret2) |
        for (@v2 <- ret2) { @12!(v2) }
      }
    }
  }
}
"#
        .to_string(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + 5,
        Some(5_000_000),
        None,
        Some(construct_deploy::DEFAULT_SEC.clone()),
        None,
        Some(merge_block.shard_id.clone()),
    )
    .unwrap();
    let probe_block = nodes[0]
        .add_block_from_deploys(std::slice::from_ref(&probe))
        .await
        .expect("validator 0 proposes the probe block");
    assert_no_failed_deploys(&probe_block);

    let rm = &nodes[0].runtime_manager;
    let first = rspace_util::get_data_at_public_channel_block(&probe_block, 11, rm).await;
    let second = rspace_util::get_data_at_public_channel_block(&probe_block, 12, rm).await;
    assert_eq!(first.len(), 1, "first consume must fire: first={first:?}");
    assert!(
        second.is_empty(),
        "leaf of \"a\" held two datums: consumed {first:?} then {second:?} \
         (write1_rejected={write1_rejected}, write2_rejected={write2_rejected})"
    );
}
