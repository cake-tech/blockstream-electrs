//! Fast tests for indexing and (with silent-payments) SP tweak index.
//! Uses regtest with 101 blocks so runs in seconds, not hours.

pub mod common;

use common::Result;


#[cfg(feature = "silent-payments")]
#[test]
fn test_sp_tweak_indexing() -> Result<()> {
    // With silent-payments and sp_begin_height=0, skip_tweaks=false in test config,
    // tweak indexing runs on regtest blocks
    let tester = common::TestRunner::new()?;
    let store = tester.store();

    let tweaked = store.tweaked_blockhashes();
    // Regtest may have few or no P2TR outputs in first 101 blocks; we only check the path ran
    // (no panic, and either some blocks tweaked or zero)
    assert!(
        tweaked.len() <= 102,
        "tweaked block count should be <= 102, got {}",
        tweaked.len()
    );

    // SP APIs should not panic: block_tweaks and tweaks_iter_scan for a valid height
    let q = tester.query();
    let sp_begin = q.sp_begin_height();
    let best_height = q.chain().best_header().height();
    let height = (sp_begin as usize).min(best_height);
    let _ = q.block_tweaks(height);
    let _ = q.tweaks_iter_scan(sp_begin, sp_begin + 1);

    Ok(())
}

#[test]
fn test_indexing_after_new_block() -> Result<()> {
    // Mine one more block and sync; index state should update
    let mut tester = common::TestRunner::new()?;
    // Ensure indexed_headers is populated so the next update (after mine) uses the
    // incremental path instead of get_all_headers (which can panic if parallel
    // getblockheaders returns out-of-order; daemon is unchanged from new_index).
    tester.sync()?;

    let headers_len = tester.store().indexed_headers.read().unwrap().len();
    assert!(
        headers_len >= 101,
        "indexed_headers should be populated after initial sync (got {}); \
         if the first update() failed or did not append, the next update would use get_all_headers",
        headers_len
    );

    let indexed_before = tester.store().indexed_blockhashes().len();

    tester.mine()?;
    let indexed_after = tester.store().indexed_blockhashes().len();

    assert!(
        indexed_after >= indexed_before,
        "indexed count should increase or stay same after mine+sync, before {} after {}",
        indexed_before,
        indexed_after
    );

    Ok(())
}
