#![cfg(feature = "s3")]
mod common;
use obelisk_storage::s3::S3;
use std::sync::Arc;

#[tokio::test]
#[ignore = "requires OBELISK_S3_BUCKET, OBELISK_S3_TEST_PREFIX and external AWS credentials"]
async fn real_s3_contract() {
    let bucket = std::env::var("OBELISK_S3_BUCKET").expect("set isolated test bucket");
    let base = std::env::var("OBELISK_S3_TEST_PREFIX").expect("set isolated test prefix");
    assert!(
        std::env::var_os("AWS_ENDPOINT").is_none()
            && std::env::var_os("AWS_ENDPOINT_URL").is_none(),
        "use an emulator-specific test for custom endpoints"
    );
    let prefix = format!("{base}/storage-{}", uuid::Uuid::new_v4());
    eprintln!("real S3 contract prefix: {prefix} (head retained)");
    common::contract(Arc::new(S3::from_env(&bucket, &prefix).unwrap())).await;
}
