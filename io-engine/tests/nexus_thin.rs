pub mod common;

use crate::common::nexus::find_nexus_by_uuid;
use common::{
    compose::{
        rpc::v1::{
            nexus::{ChildState, ChildStateReason},
            GrpcConnect,
        },
        Binary,
        Builder,
    },
    file_io,
    nexus::NexusBuilder,
    pool::PoolBuilder,
    replica::ReplicaBuilder,
};

#[tokio::test]
/// Create a nexus with a single thin-provisioned replica with size larger than
/// pool capacity, and try to write more data than pool capacity.
/// Write must fail with ENOSPC.
async fn nexus_thin_nospc_write() {
    common::composer_init();

    let test = Builder::new()
        .name("cargo-test")
        .network("10.1.0.0/16")
        .unwrap()
        .add_container_bin(
            "ms1",
            Binary::from_dbg("io-engine").with_args(vec!["-l", "1"]),
        )
        .with_clean(true)
        .build()
        .await
        .unwrap();

    let conn = GrpcConnect::new(&test);

    let mut ms1 = conn.grpc_handle("ms1").await.unwrap();

    let pool_0 = PoolBuilder::new()
        .with_name("pool0")
        .with_uuid("6e3c062c-293b-46e6-8ab3-ff13c1643437")
        .with_bdev("malloc:///mem0?size_mb=30");

    let repl_0 = ReplicaBuilder::new()
        .with_pool(&pool_0)
        .with_name("r0")
        .with_uuid("f099e2ea-61a9-40ce-a1c3-2cb13956355a")
        .with_size_mb(50)
        .with_thin(true);

    let nex_0 = NexusBuilder::new()
        .with_name("nexus0")
        .with_uuid("55b66a8f-6b4e-4a02-98c5-fb7d01f1abe5")
        .with_size_mb(50)
        .with_replica(&repl_0);

    pool_0.create(&mut ms1).await.unwrap();
    repl_0.create(&mut ms1).await.unwrap();
    nex_0.create(&mut ms1).await.unwrap();
    nex_0.publish(&mut ms1).await.unwrap();

    // Write less than pool size.
    file_io::test_write_to_nvme(&mut ms1, &nex_0.nqn(), &nex_0.serial(), 1, 30)
        .await
        .unwrap();

    // Write more than pool size. Must result in ENOSPC.
    let res = file_io::test_write_to_nvme(
        &mut ms1,
        &nex_0.nqn(),
        &nex_0.serial(),
        1,
        80,
    )
    .await;

    assert_eq!(
        res.unwrap_err().raw_os_error().unwrap(),
        libc::ENOSPC as i32
    );
}

#[tokio::test]
/// 1. Create two pools of equal size.
/// 2. Create a thick-provisioned replica occupying some space on pool #1.
/// 3. Create two thin-provisioned replica on these pools.
/// 4. Create a nexus on these two replicas.
/// 5. Write amount data less than replica size and pool capacity,
///    but more than pool #1 free space.
/// 6. First child must degrade with no space.
/// 7. Delete the thick-provisioned replica, thus freeing space on pool #1.
/// 8. Online child #1.
/// 9. Child must start rebuild.
async fn nexus_thin_local_nospc() {
    common::composer_init();

    let test = Builder::new()
        .name("cargo-test")
        .network("10.1.0.0/16")
        .unwrap()
        .add_container_bin(
            "ms1",
            Binary::from_dbg("io-engine").with_args(vec!["-l", "1"]),
        )
        .with_clean(true)
        .build()
        .await
        .unwrap();

    let conn = GrpcConnect::new(&test);

    let mut ms1 = conn.grpc_handle("ms1").await.unwrap();

    let pool_0 = PoolBuilder::new()
        .with_name("pool0")
        .with_uuid("6e3c062c-293b-46e6-8ab3-ff13c1643437")
        .with_bdev("malloc:///mem0?size_mb=80");

    let pool_1 = PoolBuilder::new()
        .with_name("pool1")
        .with_uuid("6b177ff6-0100-4456-af52-8875b1641079")
        .with_bdev("malloc:///mem1?size_mb=80");

    let repl_0 = ReplicaBuilder::new()
        .with_pool(&pool_0)
        .with_name("r0")
        .with_uuid("f099e2ea-61a9-40ce-a1c3-2cb13956355a")
        .with_size_mb(60)
        .with_thin(true);

    let fill_0 = ReplicaBuilder::new()
        .with_pool(&pool_0)
        .with_name("f0")
        .with_uuid("96d196a6-5f70-4894-8b2e-6da4b74a3c37")
        .with_size_mb(60)
        .with_thin(false);

    let repl_1 = ReplicaBuilder::new()
        .with_pool(&pool_1)
        .with_name("r1")
        .with_uuid("6466b8d5-97be-4b21-8d44-5d8cbbd6d6a0")
        .with_size_mb(60)
        .with_thin(true);

    let nex_0 = NexusBuilder::new()
        .with_name("nexus0")
        .with_uuid("55b66a8f-6b4e-4a02-98c5-fb7d01f1abe5")
        .with_size_mb(60)
        .with_replica(&repl_0)
        .with_replica(&repl_1);

    pool_0.create(&mut ms1).await.unwrap();
    pool_1.create(&mut ms1).await.unwrap();

    repl_0.create(&mut ms1).await.unwrap();
    fill_0.create(&mut ms1).await.unwrap();
    repl_1.create(&mut ms1).await.unwrap();

    nex_0.create(&mut ms1).await.unwrap();
    nex_0.publish(&mut ms1).await.unwrap();

    // Write more data than pool free space.
    // Must succeed.
    file_io::test_write_to_nvme(&mut ms1, &nex_0.nqn(), &nex_0.serial(), 5, 10)
        .await
        .unwrap();

    // First child must be degraded.
    let n = find_nexus_by_uuid(&mut ms1, &nex_0.uuid()).await.unwrap();
    assert_eq!(n.children.len(), 2);

    let child = &n.children[0];
    assert_eq!(child.state, ChildState::Degraded as i32);
    assert_eq!(child.state_reason, ChildStateReason::NoSpace as i32);

    fill_0.destroy(&mut ms1).await.unwrap();

    let n = nex_0.online_child(&mut ms1, &repl_0.bdev()).await.unwrap();
    assert_eq!(n.children.len(), 2);
    let child = &n.children[0];
    assert_eq!(child.state, ChildState::Degraded as i32);
    assert_eq!(child.state_reason, ChildStateReason::OutOfSync as i32);
}
