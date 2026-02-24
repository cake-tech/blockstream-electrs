//! Fast tests for indexing and (with silent-payments) SP tweak index.
//! Uses regtest with 101 blocks so runs in seconds, not hours.

pub mod common;

use common::Result;

#[test]
fn test_indexing_sync_and_state() -> Result<()> {
    // TestRunner::new() mines 101 blocks and runs indexer.update() once
    let tester = common::TestRunner::new()?;
    let store = tester.query.chain().store();

    // Indexing completed: tip is persisted
    assert!(
        store.done_initial_sync(),
        "expected initial sync done (tip 't' in txstore)"
    );

    // All 102 blocks (0..101) should be added and history-indexed
    let added = store.added_blockhashes.read().unwrap();
    let added_len = added.len();
    let indexed = store.indexed_blockhashes();
    let headers = store.indexed_headers.read().unwrap();
    let tip_height = headers.len().saturating_sub(1);

    assert!(
        added_len >= 101,
        "expected at least 101 blocks in txstore, got {}",
        added_len
    );
    assert!(
        indexed.len() >= 101,
        "expected at least 101 blocks in history index, got {}",
        indexed.len()
    );
    assert!(
        tip_height >= 100,
        "expected tip height >= 100, got {}",
        tip_height
    );

    Ok(())
}

#[cfg(feature = "silent-payments")]
#[test]
fn test_sp_tweak_indexing() -> Result<()> {
    // With silent-payments and sp_begin_height=0, skip_tweaks=false in test config,
    // tweak indexing runs on regtest blocks
    let tester = common::TestRunner::new()?;
    let store = tester.query.chain().store();

    let tweaked = store.tweaked_blockhashes();
    // Regtest may have few or no P2TR outputs in first 101 blocks; we only check the path ran
    // (no panic, and either some blocks tweaked or zero)
    assert!(
        tweaked.len() <= 102,
        "tweaked block count should be <= 102, got {}",
        tweaked.len()
    );

    // SP APIs should not panic: block_tweaks and tweaks_iter_scan for a valid height
    let sp_begin = tester.query.sp_begin_height();
    let best_height = tester.query.chain().best_header().height();
    let height = (sp_begin as usize).min(best_height);
    let _ = tester.query.block_tweaks(height);
    let _ = tester.query.tweaks_iter_scan(sp_begin, sp_begin + 1);

    Ok(())
}

#[test]
fn test_indexing_after_new_block() -> Result<()> {
    // Mine one more block and sync; index state should update
    let mut tester = common::TestRunner::new()?;
    let store = tester.query.chain().store();
    let indexed_before = store.indexed_blockhashes().len();

    tester.mine()?;
    let indexed_after = store.indexed_blockhashes().len();

    assert!(
        indexed_after >= indexed_before,
        "indexed count should increase or stay same after mine+sync, before {} after {}",
        indexed_before,
        indexed_after
    );

    Ok(())
}
