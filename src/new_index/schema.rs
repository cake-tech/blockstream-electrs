#[cfg(not(feature = "liquid"))]
use bitcoin::merkle_tree::MerkleBlock;
use bitcoin::{hashes::sha256d::Hash as Sha256dHash, Amount};
use bitcoin::{VarInt, Witness};
use crypto::digest::Digest;
use crypto::sha2::Sha256;
use hex::{DisplayHex, FromHex};
use itertools::Itertools;
use rayon::prelude::*;

#[cfg(not(feature = "liquid"))]
use bitcoin::consensus::encode::{deserialize, serialize};
#[cfg(feature = "liquid")]
use elements::{
    confidential,
    encode::{deserialize, serialize},
    AssetId,
};
use silentpayments::utils::receiving::{calculate_tweak_data, get_pubkey_from_input};

use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::daemon::Daemon;
use crate::errors::*;
use crate::metrics::{Gauge, HistogramOpts, HistogramTimer, HistogramVec, MetricOpts, Metrics};
use crate::util::{
    bincode, full_hash, has_prevout, is_spendable, BlockHeaderMeta, BlockId, BlockMeta,
    BlockStatus, Bytes, HeaderEntry, HeaderList, ScriptToAddr,
};
use crate::{
    chain::{BlockHash, BlockHeader, Network, OutPoint, Script, Transaction, TxOut, Txid, Value},
    daemon::tx_from_value,
};

use crate::new_index::db::{DBFlush, DBRow, ReverseScanIterator, ScanIterator, DB};
use crate::new_index::fetch::{start_fetcher, BlockEntry, FetchFrom};

#[cfg(feature = "liquid")]
use crate::elements::{asset, peg};

const MIN_HISTORY_ITEMS_TO_CACHE: usize = 100;
const MIN_SP_TWEAK_HEIGHT: usize = 823_807; // 01/01/2024

pub struct Store {
    // TODO: should be column families
    txstore_db: DB,
    history_db: DB,
    tweak_db: DB,
    cache_db: DB,
    added_blockhashes: RwLock<HashSet<BlockHash>>,
    indexed_blockhashes: RwLock<HashSet<BlockHash>>,
    tweaked_blockhashes: RwLock<HashSet<BlockHash>>,
    indexed_headers: RwLock<HeaderList>,
}

impl Store {
    pub fn open(path: &Path, config: &Config) -> Self {
        let txstore_db = DB::open(&path.join("txstore"), config);
        let added_blockhashes = load_blockhashes(&txstore_db, &BlockRow::done_filter());
        debug!("{} blocks were added", added_blockhashes.len());

        let history_db = DB::open(&path.join("history"), config);
        let indexed_blockhashes = load_blockhashes(&history_db, &BlockRow::done_filter());
        debug!("{} blocks were indexed", indexed_blockhashes.len());

        let tweak_db = DB::open(&path.join("tweak"), config);
        let tweaked_blockhashes = load_blockhashes(&tweak_db, &BlockRow::done_filter());
        debug!("{} blocks were sp tweaked", tweaked_blockhashes.len());

        let cache_db = DB::open(&path.join("cache"), config);

        let headers = if let Some(tip_hash) = txstore_db.get(b"t") {
            let tip_hash = deserialize(&tip_hash).expect("invalid chain tip in `t`");
            let headers_map = load_blockheaders(&txstore_db);
            debug!(
                "{} headers were loaded, tip at {:?}",
                headers_map.len(),
                tip_hash
            );
            HeaderList::new(headers_map, tip_hash)
        } else {
            HeaderList::empty()
        };

        Store {
            txstore_db,
            history_db,
            tweak_db,
            cache_db,
            added_blockhashes: RwLock::new(added_blockhashes),
            indexed_blockhashes: RwLock::new(indexed_blockhashes),
            tweaked_blockhashes: RwLock::new(tweaked_blockhashes),
            indexed_headers: RwLock::new(headers),
        }
    }

    pub fn txstore_db(&self) -> &DB {
        &self.txstore_db
    }

    pub fn history_db(&self) -> &DB {
        &self.history_db
    }

    pub fn tweak_db(&self) -> &DB {
        &self.tweak_db
    }

    pub fn cache_db(&self) -> &DB {
        &self.cache_db
    }

    pub fn done_initial_sync(&self) -> bool {
        self.txstore_db.get(b"t").is_some()
    }
}

type UtxoMap = HashMap<OutPoint, (BlockId, Value)>;

#[derive(Debug)]
pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    pub confirmed: Option<BlockId>,
    pub value: Value,

    #[cfg(feature = "liquid")]
    pub asset: confidential::Asset,
    #[cfg(feature = "liquid")]
    pub nonce: confidential::Nonce,
    #[cfg(feature = "liquid")]
    pub witness: elements::TxOutWitness,
}

impl From<&Utxo> for OutPoint {
    fn from(utxo: &Utxo) -> Self {
        OutPoint {
            txid: utxo.txid,
            vout: utxo.vout,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpendingInput {
    pub txid: Txid,
    pub vin: u32,
    pub confirmed: Option<BlockId>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ScriptStats {
    pub tx_count: usize,
    pub funded_txo_count: usize,
    pub spent_txo_count: usize,
    #[cfg(not(feature = "liquid"))]
    pub funded_txo_sum: u64,
    #[cfg(not(feature = "liquid"))]
    pub spent_txo_sum: u64,
}

impl ScriptStats {
    pub fn default() -> Self {
        ScriptStats {
            tx_count: 0,
            funded_txo_count: 0,
            spent_txo_count: 0,
            #[cfg(not(feature = "liquid"))]
            funded_txo_sum: 0,
            #[cfg(not(feature = "liquid"))]
            spent_txo_sum: 0,
        }
    }
}

pub struct Indexer {
    store: Arc<Store>,
    query: Arc<ChainQuery>,
    flush: DBFlush,
    from: FetchFrom,
    iconfig: IndexerConfig,
    duration: HistogramVec,
    tip_metric: Gauge,
}

struct IndexerConfig {
    light_mode: bool,
    address_search: bool,
    index_unspendables: bool,
    network: Network,
    #[cfg(feature = "liquid")]
    parent_network: crate::chain::BNetwork,
    sp_begin_height: Option<usize>,
    sp_min_dust: Option<usize>,
}

impl From<&Config> for IndexerConfig {
    fn from(config: &Config) -> Self {
        IndexerConfig {
            light_mode: config.light_mode,
            address_search: config.address_search,
            index_unspendables: config.index_unspendables,
            network: config.network_type,
            #[cfg(feature = "liquid")]
            parent_network: config.parent_network,
            sp_begin_height: config.sp_begin_height,
            sp_min_dust: config.sp_min_dust,
        }
    }
}

pub struct ChainQuery {
    store: Arc<Store>, // TODO: should be used as read-only
    daemon: Arc<Daemon>,
    light_mode: bool,
    duration: HistogramVec,
    network: Network,
}

// TODO: &[Block] should be an iterator / a queue.
impl Indexer {
    pub fn open(
        store: Arc<Store>,
        from: FetchFrom,
        config: &Config,
        metrics: &Metrics,
        query: &Arc<ChainQuery>,
    ) -> Self {
        Indexer {
            store,
            query: Arc::clone(query),
            flush: DBFlush::Disable,
            from,
            iconfig: IndexerConfig::from(config),
            duration: metrics.histogram_vec(
                HistogramOpts::new("index_duration", "Index update duration (in seconds)"),
                &["step"],
            ),
            tip_metric: metrics.gauge(MetricOpts::new("tip_height", "Current chain tip height")),
        }
    }

    fn start_timer(&self, name: &str) -> HistogramTimer {
        self.duration.with_label_values(&[name]).start_timer()
    }

    fn headers_to_add(&self, new_headers: &[HeaderEntry]) -> Vec<HeaderEntry> {
        let added_blockhashes = self.store.added_blockhashes.read().unwrap();
        new_headers
            .iter()
            .filter(|e| !added_blockhashes.contains(e.hash()))
            .cloned()
            .collect()
    }

    fn headers_to_index(&mut self, new_headers: &[HeaderEntry]) -> Vec<HeaderEntry> {
        let indexed_blockhashes = self.query.indexed_blockhashes();
        self.get_headers_to_use(indexed_blockhashes.len(), new_headers, 0)
            .iter()
            .filter(|e| !indexed_blockhashes.contains(e.hash()))
            .cloned()
            .collect()
    }

    fn headers_to_tweak(&mut self, new_headers: &[HeaderEntry]) -> Vec<HeaderEntry> {
        let tweaked_blockhashes = self.query.tweaked_blockhashes();
        let start_height = self.iconfig.sp_begin_height.unwrap_or(MIN_SP_TWEAK_HEIGHT);

        self.get_headers_to_use(tweaked_blockhashes.len(), new_headers, start_height)
            .iter()
            .filter(|e| !tweaked_blockhashes.contains(e.hash()) && e.height() >= start_height)
            .cloned()
            .collect()
    }

    fn start_auto_compactions(&self, db: &DB) {
        let key = b"F".to_vec();
        if db.get(&key).is_none() {
            db.full_compaction();
            db.put_sync(&key, b"");
            assert!(db.get(&key).is_some());
        }
        db.enable_auto_compaction();
    }

    fn get_not_indexed_headers(
        &self,
        daemon: &Daemon,
        tip: &BlockHash,
    ) -> Result<Vec<HeaderEntry>> {
        let indexed_headers = self.store.indexed_headers.read().unwrap();
        let new_headers = daemon.get_new_headers(&indexed_headers, &tip)?;
        let result = indexed_headers.order(new_headers);

        if let Some(tip) = result.last() {
            info!("{:?} ({} left to index)", tip, result.len());
        };
        Ok(result)
    }

    fn get_all_indexed_headers(&self) -> Result<Vec<HeaderEntry>> {
        let headers = self.store.indexed_headers.read().unwrap();
        let all_headers = headers.iter().cloned().collect::<Vec<_>>();

        Ok(all_headers)
    }

    fn get_headers_to_use(
        &mut self,
        lookup_len: usize,
        new_headers: &[HeaderEntry],
        start_height: usize,
    ) -> Vec<HeaderEntry> {
        let all_indexed_headers = self.get_all_indexed_headers().unwrap();
        let count_total_indexed = all_indexed_headers.len() - start_height;

        // Should have indexed more than what already has been indexed, use all headers
        if count_total_indexed > lookup_len {
            let count_left_to_index = lookup_len - count_total_indexed;

            if let FetchFrom::BlkFiles = self.from {
                if count_left_to_index < all_indexed_headers.len() / 2 {
                    self.from = FetchFrom::BlkFilesReverse;
                }
            }

            return all_indexed_headers;
        } else {
            // Just needs to index new headers
            return new_headers.to_vec();
        }
    }

    pub fn update(&mut self, daemon: &Daemon) -> Result<BlockHash> {
        let daemon = daemon.reconnect()?;
        let tip = daemon.getbestblockhash()?;
        let headers_not_indexed = self.get_not_indexed_headers(&daemon, &tip)?;

        let to_add = self.headers_to_add(&headers_not_indexed);
        if !to_add.is_empty() {
            debug!(
                "adding transactions from {} blocks using {:?}",
                to_add.len(),
                self.from
            );
            start_fetcher(self.from, &daemon, to_add)?.map(|blocks| self.add(&blocks));
            self.start_auto_compactions(&self.store.txstore_db);
        }

        let to_index = self.headers_to_index(&headers_not_indexed);
        if !to_index.is_empty() {
            debug!(
                "indexing history from {} blocks using {:?}",
                to_index.len(),
                self.from
            );
            start_fetcher(self.from, &daemon, to_index)?.map(|blocks| self.index(&blocks));
            self.start_auto_compactions(&self.store.history_db);
        }

        let to_tweak = self.headers_to_tweak(&headers_not_indexed);
        if !to_tweak.is_empty() {
            debug!(
                "indexing sp tweaks from {} blocks using {:?}",
                to_tweak.len(),
                self.from
            );
            start_fetcher(self.from, &daemon, to_tweak)?.map(|blocks| self.tweak(&blocks, &daemon));
            self.start_auto_compactions(&self.store.tweak_db);
        }

        if let DBFlush::Disable = self.flush {
            debug!("flushing to disk");
            self.store.txstore_db.flush();
            self.store.history_db.flush();
            self.flush = DBFlush::Enable;
        }

        // update the synced tip *after* the new data is flushed to disk
        debug!("updating synced tip to {:?}", tip);
        self.store.txstore_db.put_sync(b"t", &serialize(&tip));

        let mut headers = self.store.indexed_headers.write().unwrap();
        headers.apply(headers_not_indexed);
        assert_eq!(tip, *headers.tip());

        if let FetchFrom::BlkFiles = self.from {
            self.from = FetchFrom::Bitcoind;
        }

        self.tip_metric.set(headers.len() as i64 - 1);

        debug!("finished Indexer update");

        Ok(tip)
    }

    fn add(&self, blocks: &[BlockEntry]) {
        // TODO: skip orphaned blocks?
        let rows = {
            let _timer = self.start_timer("add_process");
            add_blocks(blocks, &self.iconfig)
        };
        {
            let _timer = self.start_timer("add_write");
            self.store.txstore_db.write(rows, self.flush);
        }

        self.store
            .added_blockhashes
            .write()
            .unwrap()
            .extend(blocks.iter().map(|b| b.entry.hash()));
    }

    fn index(&self, blocks: &[BlockEntry]) {
        let previous_txos_map = {
            let _timer = self.start_timer("index_lookup");
            lookup_txos(&self.store.txstore_db, &get_previous_txos(blocks), false)
        };
        let rows = {
            let _timer = self.start_timer("index_process");
            let added_blockhashes = self.store.added_blockhashes.read().unwrap();
            for b in blocks {
                let blockhash = b.entry.hash();
                // TODO: replace by lookup into txstore_db?
                if !added_blockhashes.contains(blockhash) {
                    panic!("cannot index block {} (missing from store)", blockhash);
                }
            }
            index_blocks(blocks, &previous_txos_map, &self.iconfig)
        };
        self.store.history_db.write(rows, self.flush);
    }

    fn tweak(&self, blocks: &[BlockEntry], daemon: &Daemon) {
        let _timer = self.start_timer("tweak_process");
        let tweaked_blocks = Arc::new(AtomicUsize::new(0));
        let _: Vec<_> = blocks
            .par_iter() // serialization is CPU-intensive
            .map(|b| {
                let mut rows = vec![];
                let mut tweaks: Vec<Vec<u8>> = vec![];
                let blockhash = full_hash(&b.entry.hash()[..]);
                let blockheight = b.entry.height();

                for tx in &b.block.txdata {
                    self.tweak_transaction(
                        blockheight.try_into().unwrap(),
                        tx,
                        &mut rows,
                        &mut tweaks,
                        daemon,
                    );
                }

                // persist block tweaks index:
                //      W{blockhash} → {tweak1}...{tweakN}
                rows.push(BlockRow::new_tweaks(blockhash, &tweaks).into_row());
                rows.push(BlockRow::new_done(blockhash).into_row());

                self.store.tweak_db.write(rows, self.flush);
                self.store.tweak_db.flush();

                tweaked_blocks.fetch_add(1, Ordering::SeqCst);
                info!(
                    "Sp tweaked block {} of {} total (height: {})",
                    tweaked_blocks.load(Ordering::SeqCst),
                    blocks.len(),
                    b.entry.height()
                );

                Some(())
            })
            .flatten()
            .collect();
    }

    fn tweak_transaction(
        &self,
        blockheight: u32,
        tx: &Transaction,
        rows: &mut Vec<DBRow>,
        tweaks: &mut Vec<Vec<u8>>,
        daemon: &Daemon,
    ) {
        let txid = &tx.txid();
        let mut output_pubkeys: Vec<VoutData> = Vec::with_capacity(tx.output.len());

        for (txo_index, txo) in tx.output.iter().enumerate() {
            if is_spendable(txo) {
                let amount = (txo.value as Amount).to_sat();
                if txo.script_pubkey.is_v1_p2tr()
                    && amount >= self.iconfig.sp_min_dust.unwrap_or(1_000) as u64
                {
                    output_pubkeys.push(VoutData {
                        vout: txo_index,
                        amount,
                        script_pubkey: txo.script_pubkey.clone(),
                        spending_input: self.query.lookup_spend(&OutPoint {
                            txid: txid.clone(),
                            vout: txo_index as u32,
                        }),
                    });
                }
            }
        }

        if output_pubkeys.is_empty() {
            return;
        }

        let mut pubkeys = Vec::with_capacity(tx.input.len());
        let mut outpoints = Vec::with_capacity(tx.input.len());

        for txin in tx.input.iter() {
            let prev_txid = txin.previous_output.txid;
            let prev_vout = txin.previous_output.vout;

            // Collect outpoints from all of the inputs, not just the silent payment eligible
            // inputs. This is relevant for transactions that have a mix of silent payments
            // eligible and non-eligible inputs, where the smallest outpoint is for one of the
            // non-eligible inputs
            outpoints.push((prev_txid.to_string(), prev_vout));

            let prev_tx_result = daemon.gettransaction_raw(&prev_txid, None, true);
            if let Ok(prev_tx_value) = prev_tx_result {
                if let Some(prev_tx) = tx_from_value(prev_tx_value.get("hex").unwrap().clone()).ok()
                {
                    if let Some(prevout) = prev_tx.output.get(prev_vout as usize) {
                        match get_pubkey_from_input(
                            &txin.script_sig.to_bytes(),
                            &(txin.witness.clone() as Witness).to_vec(),
                            &prevout.script_pubkey.to_bytes(),
                        ) {
                            Ok(Some(pubkey)) => pubkeys.push(pubkey),
                            Ok(None) => (),
                            Err(_e) => {}
                        }
                    }
                }
            }
        }

        let pubkeys_ref: Vec<_> = pubkeys.iter().collect();
        if !pubkeys_ref.is_empty() {
            if let Some(tweak) = calculate_tweak_data(&pubkeys_ref, &outpoints).ok() {
                // persist tweak index:
                //      K{blockhash}{txid} → {tweak}{serialized-vout-data}
                rows.push(
                    TweakTxRow::new(
                        blockheight,
                        txid.clone(),
                        &TweakData {
                            tweak: tweak.serialize().to_lower_hex_string(),
                            vout_data: output_pubkeys.clone(),
                        },
                    )
                    .into_row(),
                );
                tweaks.push(tweak.serialize().to_vec());
            }
        }
    }

    pub fn fetch_from(&mut self, from: FetchFrom) {
        self.from = from;
    }
}

impl ChainQuery {
    pub fn new(store: Arc<Store>, daemon: Arc<Daemon>, config: &Config, metrics: &Metrics) -> Self {
        ChainQuery {
            store,
            daemon,
            light_mode: config.light_mode,
            network: config.network_type,
            duration: metrics.histogram_vec(
                HistogramOpts::new("query_duration", "Index query duration (in seconds)"),
                &["name"],
            ),
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn start_timer(&self, name: &str) -> HistogramTimer {
        self.duration.with_label_values(&[name]).start_timer()
    }

    pub fn get_block_txids(&self, hash: &BlockHash) -> Option<Vec<Txid>> {
        let _timer = self.start_timer("get_block_txids");

        if self.light_mode {
            // TODO fetch block as binary from REST API instead of as hex
            let mut blockinfo = self.daemon.getblock_raw(hash, 1).ok()?;
            Some(serde_json::from_value(blockinfo["tx"].take()).unwrap())
        } else {
            self.store
                .txstore_db
                .get(&BlockRow::txids_key(full_hash(&hash[..])))
                .map(|val| bincode::deserialize_little(&val).expect("failed to parse block txids"))
        }
    }

    pub fn get_block_meta(&self, hash: &BlockHash) -> Option<BlockMeta> {
        let _timer = self.start_timer("get_block_meta");

        if self.light_mode {
            let blockinfo = self.daemon.getblock_raw(hash, 1).ok()?;
            Some(serde_json::from_value(blockinfo).unwrap())
        } else {
            self.store
                .txstore_db
                .get(&BlockRow::meta_key(full_hash(&hash[..])))
                .map(|val| bincode::deserialize_little(&val).expect("failed to parse BlockMeta"))
        }
    }

    pub fn get_block_raw(&self, hash: &BlockHash) -> Option<Vec<u8>> {
        let _timer = self.start_timer("get_block_raw");

        if self.light_mode {
            let blockval = self.daemon.getblock_raw(hash, 0).ok()?;
            let blockhex = blockval.as_str().expect("valid block from bitcoind");
            Some(Vec::from_hex(blockhex).expect("valid block from bitcoind"))
        } else {
            let entry = self.header_by_hash(hash)?;
            let meta = self.get_block_meta(hash)?;
            let txids = self.get_block_txids(hash)?;

            // Reconstruct the raw block using the header and txids,
            // as <raw header><tx count varint><raw txs>
            let mut raw = Vec::with_capacity(meta.size as usize);

            raw.append(&mut serialize(entry.header()));
            raw.append(&mut serialize(&VarInt(txids.len() as u64)));

            for txid in txids {
                // we don't need to provide the blockhash because we know we're not in light mode
                raw.append(&mut self.lookup_raw_txn(&txid, None)?);
            }

            Some(raw)
        }
    }

    pub fn get_block_header(&self, hash: &BlockHash) -> Option<BlockHeader> {
        let _timer = self.start_timer("get_block_header");
        Some(self.header_by_hash(hash)?.header().clone())
    }

    pub fn get_mtp(&self, height: usize) -> u32 {
        let _timer = self.start_timer("get_block_mtp");
        self.store.indexed_headers.read().unwrap().get_mtp(height)
    }

    pub fn get_block_with_meta(&self, hash: &BlockHash) -> Option<BlockHeaderMeta> {
        let _timer = self.start_timer("get_block_with_meta");
        let header_entry = self.header_by_hash(hash)?;
        Some(BlockHeaderMeta {
            meta: self.get_block_meta(hash)?,
            mtp: self.get_mtp(header_entry.height()),
            header_entry,
        })
    }

    pub fn history_iter_scan(&self, code: u8, hash: &[u8], start_height: usize) -> ScanIterator {
        self.store.history_db.iter_scan_from(
            &TxHistoryRow::filter(code, &hash[..]),
            &TxHistoryRow::prefix_height(code, &hash[..], start_height as u32),
        )
    }
    fn history_iter_scan_reverse(&self, code: u8, hash: &[u8]) -> ReverseScanIterator {
        self.store.history_db.iter_scan_reverse(
            &TxHistoryRow::filter(code, &hash[..]),
            &TxHistoryRow::prefix_end(code, &hash[..]),
        )
    }

    pub fn history(
        &self,
        scripthash: &[u8],
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<(Transaction, BlockId)> {
        // scripthash lookup
        self._history(b'H', scripthash, last_seen_txid, limit)
    }

    fn _history(
        &self,
        code: u8,
        hash: &[u8],
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<(Transaction, BlockId)> {
        let _timer_scan = self.start_timer("history");
        let txs_conf = self
            .history_iter_scan_reverse(code, hash)
            .map(|row| TxHistoryRow::from_row(row).get_txid())
            // XXX: unique() requires keeping an in-memory list of all txids, can we avoid that?
            .unique()
            // TODO seek directly to last seen tx without reading earlier rows
            .skip_while(|txid| {
                // skip until we reach the last_seen_txid
                last_seen_txid.map_or(false, |last_seen_txid| last_seen_txid != txid)
            })
            .skip(match last_seen_txid {
                Some(_) => 1, // skip the last_seen_txid itself
                None => 0,
            })
            .filter_map(|txid| self.tx_confirming_block(&txid).map(|b| (txid, b)))
            .take(limit)
            .collect::<Vec<(Txid, BlockId)>>();

        self.lookup_txns(&txs_conf)
            .expect("failed looking up txs in history index")
            .into_iter()
            .zip(txs_conf)
            .map(|(tx, (_, blockid))| (tx, blockid))
            .collect()
    }

    pub fn history_txids(&self, scripthash: &[u8], limit: usize) -> Vec<(Txid, BlockId)> {
        // scripthash lookup
        self._history_txids(b'H', scripthash, limit)
    }

    fn _history_txids(&self, code: u8, hash: &[u8], limit: usize) -> Vec<(Txid, BlockId)> {
        let _timer = self.start_timer("history_txids");
        self.history_iter_scan(code, hash, 0)
            .map(|row| TxHistoryRow::from_row(row).get_txid())
            .unique()
            .filter_map(|txid| self.tx_confirming_block(&txid).map(|b| (txid, b)))
            .take(limit)
            .collect()
    }

    pub fn store_tweak_cache_height(&self, height: u32, tip: u32) {
        self.store.tweak_db.put_sync(
            &TweakBlockRecordCacheRow::key(height),
            &TweakBlockRecordCacheRow::value(tip),
        );
    }

    pub fn get_tweak_cached_height(&self, height: u32) -> Option<u32> {
        self.store
            .tweak_db
            .iter_scan(&TweakBlockRecordCacheRow::key(height))
            .map(|v| TweakBlockRecordCacheRow::from_row(v).value)
            .next()
    }

    fn tweaks_iter_scan(&self, code: u8, height: u32) -> ScanIterator {
        self.store.tweak_db.iter_scan_from(
            &TweakTxRow::filter(code),
            &TweakTxRow::prefix_blockheight(code, height),
        )
    }

    pub fn tweaks(&self, height: u32) -> Vec<(Txid, TweakData)> {
        self._tweaks(b'K', height)
    }

    fn _tweaks(&self, code: u8, height: u32) -> Vec<(Txid, TweakData)> {
        let _timer = self.start_timer("tweaks");
        self.tweaks_iter_scan(code, height)
            .filter_map(|row| {
                let tweak_row = TweakTxRow::from_row(row);
                if height != tweak_row.key.blockheight {
                    return None;
                }

                let txid = tweak_row.key.txid;
                let tweak = tweak_row.get_tweak_data();
                Some((txid, tweak))
            })
            .collect()
    }

    pub fn indexed_blockhashes(&self) -> HashSet<BlockHash> {
        load_blockhashes(&self.store.history_db, &BlockRow::done_filter())
    }

    pub fn tweaked_blockhashes(&self) -> HashSet<BlockHash> {
        load_blockhashes(&self.store.tweak_db, &BlockRow::done_filter())
    }

    // TODO: avoid duplication with stats/stats_delta?
    pub fn utxo(&self, scripthash: &[u8], limit: usize) -> Result<Vec<Utxo>> {
        let _timer = self.start_timer("utxo");

        // get the last known utxo set and the blockhash it was updated for.
        // invalidates the cache if the block was orphaned.
        let cache: Option<(UtxoMap, usize)> = self
            .store
            .cache_db
            .get(&UtxoCacheRow::key(scripthash))
            .map(|c| bincode::deserialize_little(&c).unwrap())
            .and_then(|(utxos_cache, blockhash)| {
                self.height_by_hash(&blockhash)
                    .map(|height| (utxos_cache, height))
            })
            .map(|(utxos_cache, height)| (from_utxo_cache(utxos_cache, self), height));
        let had_cache = cache.is_some();

        // update utxo set with new transactions since
        let (newutxos, lastblock, processed_items) = cache.map_or_else(
            || self.utxo_delta(scripthash, HashMap::new(), 0, limit),
            |(oldutxos, blockheight)| self.utxo_delta(scripthash, oldutxos, blockheight + 1, limit),
        )?;

        // save updated utxo set to cache
        if let Some(lastblock) = lastblock {
            if had_cache || processed_items > MIN_HISTORY_ITEMS_TO_CACHE {
                self.store.cache_db.write(
                    vec![UtxoCacheRow::new(scripthash, &newutxos, &lastblock).into_row()],
                    DBFlush::Enable,
                );
            }
        }

        // format as Utxo objects
        Ok(newutxos
            .into_iter()
            .map(|(outpoint, (blockid, value))| {
                // in elements/liquid chains, we have to lookup the txo in order to get its
                // associated asset. the asset information could be kept in the db history rows
                // alongside the value to avoid this.
                #[cfg(feature = "liquid")]
                let txo = self.lookup_txo(&outpoint).expect("missing utxo");

                Utxo {
                    txid: outpoint.txid,
                    vout: outpoint.vout,
                    value,
                    confirmed: Some(blockid),

                    #[cfg(feature = "liquid")]
                    asset: txo.asset,
                    #[cfg(feature = "liquid")]
                    nonce: txo.nonce,
                    #[cfg(feature = "liquid")]
                    witness: txo.witness,
                }
            })
            .collect())
    }

    fn utxo_delta(
        &self,
        scripthash: &[u8],
        init_utxos: UtxoMap,
        start_height: usize,
        limit: usize,
    ) -> Result<(UtxoMap, Option<BlockHash>, usize)> {
        let _timer = self.start_timer("utxo_delta");
        let history_iter = self
            .history_iter_scan(b'H', scripthash, start_height)
            .map(TxHistoryRow::from_row)
            .filter_map(|history| {
                self.tx_confirming_block(&history.get_txid())
                    .map(|b| (history, b))
            });

        let mut utxos = init_utxos;
        let mut processed_items = 0;
        let mut lastblock = None;

        for (history, blockid) in history_iter {
            processed_items += 1;
            lastblock = Some(blockid.hash);

            match history.key.txinfo {
                TxHistoryInfo::Funding(ref info) => {
                    utxos.insert(history.get_funded_outpoint(), (blockid, info.value))
                }
                TxHistoryInfo::Spending(_) => utxos.remove(&history.get_funded_outpoint()),
                #[cfg(feature = "liquid")]
                TxHistoryInfo::Issuing(_)
                | TxHistoryInfo::Burning(_)
                | TxHistoryInfo::Pegin(_)
                | TxHistoryInfo::Pegout(_) => unreachable!(),
            };

            // abort if the utxo set size excedees the limit at any point in time
            if utxos.len() > limit {
                bail!(ErrorKind::TooPopular)
            }
        }

        Ok((utxos, lastblock, processed_items))
    }

    pub fn stats(&self, scripthash: &[u8]) -> ScriptStats {
        let _timer = self.start_timer("stats");

        // get the last known stats and the blockhash they are updated for.
        // invalidates the cache if the block was orphaned.
        let cache: Option<(ScriptStats, usize)> = self
            .store
            .cache_db
            .get(&StatsCacheRow::key(scripthash))
            .map(|c| bincode::deserialize_little(&c).unwrap())
            .and_then(|(stats, blockhash)| {
                self.height_by_hash(&blockhash)
                    .map(|height| (stats, height))
            });

        // update stats with new transactions since
        let (newstats, lastblock) = cache.map_or_else(
            || self.stats_delta(scripthash, ScriptStats::default(), 0),
            |(oldstats, blockheight)| self.stats_delta(scripthash, oldstats, blockheight + 1),
        );

        // save updated stats to cache
        if let Some(lastblock) = lastblock {
            if newstats.funded_txo_count + newstats.spent_txo_count > MIN_HISTORY_ITEMS_TO_CACHE {
                self.store.cache_db.write(
                    vec![StatsCacheRow::new(scripthash, &newstats, &lastblock).into_row()],
                    DBFlush::Enable,
                );
            }
        }

        newstats
    }

    fn stats_delta(
        &self,
        scripthash: &[u8],
        init_stats: ScriptStats,
        start_height: usize,
    ) -> (ScriptStats, Option<BlockHash>) {
        let _timer = self.start_timer("stats_delta"); // TODO: measure also the number of txns processed.
        let history_iter = self
            .history_iter_scan(b'H', scripthash, start_height)
            .map(TxHistoryRow::from_row)
            .filter_map(|history| {
                self.tx_confirming_block(&history.get_txid())
                    // drop history entries that were previously confirmed in a re-orged block and later
                    // confirmed again at a different height
                    .filter(|blockid| blockid.height == history.key.confirmed_height as usize)
                    .map(|blockid| (history, blockid))
            });

        let mut stats = init_stats;
        let mut seen_txids = HashSet::new();
        let mut lastblock = None;

        for (history, blockid) in history_iter {
            if lastblock != Some(blockid.hash) {
                seen_txids.clear();
            }

            if seen_txids.insert(history.get_txid()) {
                stats.tx_count += 1;
            }

            match history.key.txinfo {
                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Funding(ref info) => {
                    stats.funded_txo_count += 1;
                    stats.funded_txo_sum += info.value;
                }

                #[cfg(not(feature = "liquid"))]
                TxHistoryInfo::Spending(ref info) => {
                    stats.spent_txo_count += 1;
                    stats.spent_txo_sum += info.value;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Funding(_) => {
                    stats.funded_txo_count += 1;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Spending(_) => {
                    stats.spent_txo_count += 1;
                }

                #[cfg(feature = "liquid")]
                TxHistoryInfo::Issuing(_)
                | TxHistoryInfo::Burning(_)
                | TxHistoryInfo::Pegin(_)
                | TxHistoryInfo::Pegout(_) => unreachable!(),
            }

            lastblock = Some(blockid.hash);
        }

        (stats, lastblock)
    }

    pub fn address_search(&self, prefix: &str, limit: usize) -> Vec<String> {
        let _timer_scan = self.start_timer("address_search");
        self.store
            .history_db
            .iter_scan(&addr_search_filter(prefix))
            .take(limit)
            .map(|row| std::str::from_utf8(&row.key[1..]).unwrap().to_string())
            .collect()
    }

    fn header_by_hash(&self, hash: &BlockHash) -> Option<HeaderEntry> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .cloned()
    }

    // Get the height of a blockhash, only if its part of the best chain
    pub fn height_by_hash(&self, hash: &BlockHash) -> Option<usize> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .map(|header| header.height())
    }

    pub fn header_by_height(&self, height: usize) -> Option<HeaderEntry> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .cloned()
    }

    pub fn get_block_tweaks(&self, hash: &BlockHash) -> Option<Vec<Vec<u8>>> {
        let _timer = self.start_timer("get_block_tweaks");

        self.store
            .tweak_db
            .get(&BlockRow::tweaks_key(full_hash(&hash[..])))
            .map(|val| bincode::deserialize_little(&val).expect("failed to parse block tweaks"))
    }

    pub fn hash_by_height(&self, height: usize) -> Option<BlockHash> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .map(|entry| *entry.hash())
    }

    pub fn blockid_by_height(&self, height: usize) -> Option<BlockId> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_height(height)
            .map(BlockId::from)
    }

    // returns None for orphaned blocks
    pub fn blockid_by_hash(&self, hash: &BlockHash) -> Option<BlockId> {
        self.store
            .indexed_headers
            .read()
            .unwrap()
            .header_by_blockhash(hash)
            .map(BlockId::from)
    }

    pub fn best_height(&self) -> usize {
        self.store.indexed_headers.read().unwrap().len() - 1
    }

    pub fn best_hash(&self) -> BlockHash {
        *self.store.indexed_headers.read().unwrap().tip()
    }

    pub fn best_header(&self) -> HeaderEntry {
        let headers = self.store.indexed_headers.read().unwrap();
        headers
            .header_by_blockhash(headers.tip())
            .expect("missing chain tip")
            .clone()
    }

    // TODO: can we pass txids as a "generic iterable"?
    // TODO: should also use a custom ThreadPoolBuilder?
    pub fn lookup_txns(&self, txids: &[(Txid, BlockId)]) -> Result<Vec<Transaction>> {
        let _timer = self.start_timer("lookup_txns");
        txids
            .par_iter()
            .map(|(txid, blockid)| {
                self.lookup_txn(txid, Some(&blockid.hash))
                    .chain_err(|| "missing tx")
            })
            .collect::<Result<Vec<Transaction>>>()
    }

    pub fn lookup_txn(&self, txid: &Txid, blockhash: Option<&BlockHash>) -> Option<Transaction> {
        let _timer = self.start_timer("lookup_txn");
        self.lookup_raw_txn(txid, blockhash).map(|rawtx| {
            let txn: Transaction = deserialize(&rawtx).expect("failed to parse Transaction");
            assert_eq!(*txid, txn.txid());
            txn
        })
    }

    pub fn lookup_raw_txn(&self, txid: &Txid, blockhash: Option<&BlockHash>) -> Option<Bytes> {
        let _timer = self.start_timer("lookup_raw_txn");

        if self.light_mode {
            let queried_blockhash =
                blockhash.map_or_else(|| self.tx_confirming_block(txid).map(|b| b.hash), |_| None);
            let blockhash = blockhash.or_else(|| queried_blockhash.as_ref())?;
            // TODO fetch transaction as binary from REST API instead of as hex
            let txval = self
                .daemon
                .gettransaction_raw(txid, Some(blockhash), false)
                .ok()?;
            let txhex = txval.as_str().expect("valid tx from bitcoind");
            Some(Bytes::from_hex(txhex).expect("valid tx from bitcoind"))
        } else {
            self.store.txstore_db.get(&TxRow::key(&txid[..]))
        }
    }

    pub fn lookup_txo(&self, outpoint: &OutPoint) -> Option<TxOut> {
        let _timer = self.start_timer("lookup_txo");
        lookup_txo(&self.store.txstore_db, outpoint)
    }

    pub fn lookup_txos(&self, outpoints: &BTreeSet<OutPoint>) -> HashMap<OutPoint, TxOut> {
        let _timer = self.start_timer("lookup_txos");
        lookup_txos(&self.store.txstore_db, outpoints, false)
    }

    pub fn lookup_avail_txos(&self, outpoints: &BTreeSet<OutPoint>) -> HashMap<OutPoint, TxOut> {
        let _timer = self.start_timer("lookup_available_txos");
        lookup_txos(&self.store.txstore_db, outpoints, true)
    }

    pub fn lookup_spend(&self, outpoint: &OutPoint) -> Option<SpendingInput> {
        let _timer = self.start_timer("lookup_spend");
        self.store
            .history_db
            .iter_scan(&TxEdgeRow::filter(&outpoint))
            .map(TxEdgeRow::from_row)
            .find_map(|edge| {
                let txid: Txid = deserialize(&edge.key.spending_txid).unwrap();
                self.tx_confirming_block(&txid).map(|b| SpendingInput {
                    txid,
                    vin: edge.key.spending_vin as u32,
                    confirmed: Some(b),
                })
            })
    }
    pub fn tx_confirming_block(&self, txid: &Txid) -> Option<BlockId> {
        let _timer = self.start_timer("tx_confirming_block");
        let headers = self.store.indexed_headers.read().unwrap();
        self.store
            .txstore_db
            .iter_scan(&TxConfRow::filter(&txid[..]))
            .map(TxConfRow::from_row)
            // header_by_blockhash only returns blocks that are part of the best chain,
            // or None for orphaned blocks.
            .filter_map(|conf| {
                headers.header_by_blockhash(&deserialize(&conf.key.blockhash).unwrap())
            })
            .next()
            .map(BlockId::from)
    }

    pub fn get_block_status(&self, hash: &BlockHash) -> BlockStatus {
        // TODO differentiate orphaned and non-existing blocks? telling them apart requires
        // an additional db read.

        let headers = self.store.indexed_headers.read().unwrap();

        // header_by_blockhash only returns blocks that are part of the best chain,
        // or None for orphaned blocks.
        headers
            .header_by_blockhash(hash)
            .map_or_else(BlockStatus::orphaned, |header| {
                BlockStatus::confirmed(
                    header.height(),
                    headers
                        .header_by_height(header.height() + 1)
                        .map(|h| *h.hash()),
                )
            })
    }

    #[cfg(not(feature = "liquid"))]
    pub fn get_merkleblock_proof(&self, txid: &Txid) -> Option<MerkleBlock> {
        let _timer = self.start_timer("get_merkleblock_proof");
        let blockid = self.tx_confirming_block(txid)?;
        let headerentry = self.header_by_hash(&blockid.hash)?;
        let block_txids = self.get_block_txids(&blockid.hash)?;

        Some(MerkleBlock::from_header_txids_with_predicate(
            headerentry.header(),
            &block_txids,
            |t| t == txid,
        ))
    }

    #[cfg(feature = "liquid")]
    pub fn asset_history(
        &self,
        asset_id: &AssetId,
        last_seen_txid: Option<&Txid>,
        limit: usize,
    ) -> Vec<(Transaction, BlockId)> {
        self._history(b'I', &asset_id.into_inner()[..], last_seen_txid, limit)
    }

    #[cfg(feature = "liquid")]
    pub fn asset_history_txids(&self, asset_id: &AssetId, limit: usize) -> Vec<(Txid, BlockId)> {
        self._history_txids(b'I', &asset_id.into_inner()[..], limit)
    }
}

fn load_blockhashes(db: &DB, prefix: &[u8]) -> HashSet<BlockHash> {
    db.iter_scan(prefix)
        .map(BlockRow::from_row)
        .map(|r| deserialize(&r.key.hash).expect("failed to parse BlockHash"))
        .collect()
}

fn load_blockheaders(db: &DB) -> HashMap<BlockHash, BlockHeader> {
    db.iter_scan(&BlockRow::header_filter())
        .map(BlockRow::from_row)
        .map(|r| {
            let key: BlockHash = deserialize(&r.key.hash).expect("failed to parse BlockHash");
            let value: BlockHeader = deserialize(&r.value).expect("failed to parse BlockHeader");
            (key, value)
        })
        .collect()
}

fn add_blocks(block_entries: &[BlockEntry], iconfig: &IndexerConfig) -> Vec<DBRow> {
    // persist individual transactions:
    //      T{txid} → {rawtx}
    //      C{txid}{blockhash}{height} →
    //      O{txid}{index} → {txout}
    // persist block headers', block txids' and metadata rows:
    //      B{blockhash} → {header}
    //      X{blockhash} → {txid1}...{txidN}
    //      M{blockhash} → {tx_count}{size}{weight}
    block_entries
        .par_iter() // serialization is CPU-intensive
        .map(|b| {
            let mut rows = vec![];
            let blockhash = full_hash(&b.entry.hash()[..]);
            let txids: Vec<Txid> = b.block.txdata.iter().map(|tx| tx.txid()).collect();
            for tx in &b.block.txdata {
                add_transaction(tx, blockhash, &mut rows, iconfig);
            }

            if !iconfig.light_mode {
                rows.push(BlockRow::new_txids(blockhash, &txids).into_row());
                rows.push(BlockRow::new_meta(blockhash, &BlockMeta::from(b)).into_row());
            }

            rows.push(BlockRow::new_header(&b).into_row());
            rows.push(BlockRow::new_done(blockhash).into_row()); // mark block as "added"
            rows
        })
        .flatten()
        .collect()
}

fn add_transaction(
    tx: &Transaction,
    blockhash: FullHash,
    rows: &mut Vec<DBRow>,
    iconfig: &IndexerConfig,
) {
    rows.push(TxConfRow::new(tx, blockhash).into_row());

    if !iconfig.light_mode {
        rows.push(TxRow::new(tx).into_row());
    }

    let txid = full_hash(&tx.txid()[..]);
    for (txo_index, txo) in tx.output.iter().enumerate() {
        if is_spendable(txo) {
            rows.push(TxOutRow::new(&txid, txo_index, txo).into_row());
        }
    }
}

fn get_previous_txos(block_entries: &[BlockEntry]) -> BTreeSet<OutPoint> {
    block_entries
        .iter()
        .flat_map(|b| b.block.txdata.iter())
        .flat_map(|tx| {
            tx.input
                .iter()
                .filter(|txin| has_prevout(txin))
                .map(|txin| txin.previous_output)
        })
        .collect()
}

fn lookup_txos(
    txstore_db: &DB,
    outpoints: &BTreeSet<OutPoint>,
    allow_missing: bool,
) -> HashMap<OutPoint, TxOut> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(16) // we need to saturate SSD IOPS
        .thread_name(|i| format!("lookup-txo-{}", i))
        .build()
        .unwrap();
    pool.install(|| {
        outpoints
            .par_iter()
            .filter_map(|outpoint| {
                lookup_txo(&txstore_db, &outpoint)
                    .or_else(|| {
                        if !allow_missing {
                            panic!("missing txo {} in {:?}", outpoint, txstore_db);
                        }
                        None
                    })
                    .map(|txo| (*outpoint, txo))
            })
            .collect()
    })
}

fn lookup_txo(txstore_db: &DB, outpoint: &OutPoint) -> Option<TxOut> {
    txstore_db
        .get(&TxOutRow::key(&outpoint))
        .map(|val| deserialize(&val).expect("failed to parse TxOut"))
}

fn index_blocks(
    block_entries: &[BlockEntry],
    previous_txos_map: &HashMap<OutPoint, TxOut>,
    iconfig: &IndexerConfig,
) -> Vec<DBRow> {
    block_entries
        .par_iter() // serialization is CPU-intensive
        .map(|b| {
            let mut rows = vec![];
            for tx in &b.block.txdata {
                let height = b.entry.height() as u32;
                index_transaction(tx, height, previous_txos_map, &mut rows, iconfig);
            }
            rows.push(BlockRow::new_done(full_hash(&b.entry.hash()[..])).into_row()); // mark block as "indexed"
            rows
        })
        .flatten()
        .collect()
}

#[derive(Serialize, Deserialize)]
struct TweakBlockRecordCacheKey {
    code: u8,
    height: u32,
}

struct TweakBlockRecordCacheRow {
    key: TweakBlockRecordCacheKey,
    value: u32, // last_height when the tweak cache was updated
}

impl TweakBlockRecordCacheRow {
    fn new(height: u32, tip: u32) -> Self {
        TweakBlockRecordCacheRow {
            key: TweakBlockRecordCacheKey { code: b'B', height },
            value: tip,
        }
    }

    pub fn key(height: u32) -> Bytes {
        bincode::serialize_big(&TweakBlockRecordCacheKey { code: b'B', height }).unwrap()
    }

    pub fn value(tip: u32) -> Bytes {
        bincode::serialize_big(&tip).unwrap()
    }

    pub fn from_row(row: DBRow) -> TweakBlockRecordCacheRow {
        let key: TweakBlockRecordCacheKey = bincode::deserialize_big(&row.key).unwrap();
        let value: u32 = bincode::deserialize_big(&row.value).unwrap();
        TweakBlockRecordCacheRow { key, value }
    }

    fn into_row(self) -> DBRow {
        let TweakBlockRecordCacheRow { key, value } = self;
        DBRow {
            key: bincode::serialize_big(&key).unwrap(),
            value: bincode::serialize_big(&value).unwrap(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoutData {
    pub vout: usize,
    pub amount: u64,
    pub script_pubkey: Script,
    pub spending_input: Option<SpendingInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TweakData {
    pub tweak: String,
    pub vout_data: Vec<VoutData>,
}

#[derive(Serialize, Deserialize)]
pub struct TweakTxKey {
    code: u8,
    blockheight: u32,
    txid: Txid,
}

pub struct TweakTxRow {
    pub key: TweakTxKey,
    pub value: TweakData,
}

impl TweakTxRow {
    pub fn new(blockheight: u32, txid: Txid, tweak: &TweakData) -> TweakTxRow {
        TweakTxRow {
            key: TweakTxKey {
                code: b'K',
                blockheight,
                txid,
            },
            value: tweak.clone(),
        }
    }

    fn into_row(self) -> DBRow {
        let TweakTxRow { key, value } = self;
        DBRow {
            key: bincode::serialize_big(&key).unwrap(),
            value: bincode::serialize_big(&value).unwrap(),
        }
    }

    fn from_row(row: DBRow) -> TweakTxRow {
        let key: TweakTxKey = bincode::deserialize_big(&row.key).unwrap();
        let value: TweakData = bincode::deserialize_big(&row.value).unwrap();
        TweakTxRow { key, value }
    }

    fn filter(code: u8) -> Bytes {
        [code].to_vec()
    }

    fn prefix_blockheight(code: u8, height: u32) -> Bytes {
        bincode::serialize_big(&(code, height)).unwrap()
    }

    pub fn get_tweak_data(&self) -> TweakData {
        self.value.clone()
    }
}

// TODO: return an iterator?
fn index_transaction(
    tx: &Transaction,
    confirmed_height: u32,
    previous_txos_map: &HashMap<OutPoint, TxOut>,
    rows: &mut Vec<DBRow>,
    iconfig: &IndexerConfig,
) {
    // persist history index:
    //      H{funding-scripthash}{funding-height}F{funding-txid:vout} → ""
    //      H{funding-scripthash}{spending-height}S{spending-txid:vin}{funding-txid:vout} → ""
    // persist "edges" for fast is-this-TXO-spent check
    //      S{funding-txid:vout}{spending-txid:vin} → ""
    let txid = full_hash(&tx.txid()[..]);
    for (txo_index, txo) in tx.output.iter().enumerate() {
        if is_spendable(txo) || iconfig.index_unspendables {
            let history = TxHistoryRow::new(
                &txo.script_pubkey,
                confirmed_height,
                TxHistoryInfo::Funding(FundingInfo {
                    txid,
                    vout: txo_index as u16,
                    value: txo.value.amount_value(),
                }),
            );
            rows.push(history.into_row());

            // for prefix address search, only saved when --address-search is enabled
            //      a{funding-address-str} → ""
            if iconfig.address_search {
                if let Some(row) = addr_search_row(&txo.script_pubkey, iconfig.network) {
                    rows.push(row);
                }
            }
        }
    }
    for (txi_index, txi) in tx.input.iter().enumerate() {
        if !has_prevout(txi) {
            continue;
        }
        let prev_txo = previous_txos_map
            .get(&txi.previous_output)
            .unwrap_or_else(|| panic!("missing previous txo {}", txi.previous_output));

        let history = TxHistoryRow::new(
            &prev_txo.script_pubkey,
            confirmed_height,
            TxHistoryInfo::Spending(SpendingInfo {
                txid,
                vin: txi_index as u16,
                prev_txid: full_hash(&txi.previous_output.txid[..]),
                prev_vout: txi.previous_output.vout as u16,
                value: prev_txo.value.amount_value(),
            }),
        );
        rows.push(history.into_row());

        let edge = TxEdgeRow::new(
            full_hash(&txi.previous_output.txid[..]),
            txi.previous_output.vout as u16,
            txid,
            txi_index as u16,
        );
        rows.push(edge.into_row());
    }

    // Index issued assets & native asset pegins/pegouts/burns
    #[cfg(feature = "liquid")]
    asset::index_confirmed_tx_assets(
        tx,
        confirmed_height,
        iconfig.network,
        iconfig.parent_network,
        rows,
    );
}

fn addr_search_row(spk: &Script, network: Network) -> Option<DBRow> {
    spk.to_address_str(network).map(|address| DBRow {
        key: [b"a", address.as_bytes()].concat(),
        value: vec![],
    })
}

fn addr_search_filter(prefix: &str) -> Bytes {
    [b"a", prefix.as_bytes()].concat()
}

// TODO: replace by a separate opaque type (similar to Sha256dHash, but without the "double")
pub type FullHash = [u8; 32]; // serialized SHA256 result

pub fn compute_script_hash(script: &Script) -> FullHash {
    let mut hash = FullHash::default();
    let mut sha2 = Sha256::new();
    sha2.input(script.as_bytes());
    sha2.result(&mut hash);
    hash
}

pub fn parse_hash(hash: &FullHash) -> Sha256dHash {
    deserialize(hash).expect("failed to parse Sha256dHash")
}

#[derive(Serialize, Deserialize)]
struct TxRowKey {
    code: u8,
    txid: FullHash,
}

struct TxRow {
    key: TxRowKey,
    value: Bytes, // raw transaction
}

impl TxRow {
    fn new(txn: &Transaction) -> TxRow {
        let txid = full_hash(&txn.txid()[..]);
        TxRow {
            key: TxRowKey { code: b'T', txid },
            value: serialize(txn),
        }
    }

    fn key(prefix: &[u8]) -> Bytes {
        [b"T", prefix].concat()
    }

    fn into_row(self) -> DBRow {
        let TxRow { key, value } = self;
        DBRow {
            key: bincode::serialize_little(&key).unwrap(),
            value,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxConfKey {
    code: u8,
    txid: FullHash,
    blockhash: FullHash,
}

struct TxConfRow {
    key: TxConfKey,
}

impl TxConfRow {
    fn new(txn: &Transaction, blockhash: FullHash) -> TxConfRow {
        let txid = full_hash(&txn.txid()[..]);
        TxConfRow {
            key: TxConfKey {
                code: b'C',
                txid,
                blockhash,
            },
        }
    }

    fn filter(prefix: &[u8]) -> Bytes {
        [b"C", prefix].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: vec![],
        }
    }

    fn from_row(row: DBRow) -> Self {
        TxConfRow {
            key: bincode::deserialize_little(&row.key).expect("failed to parse TxConfKey"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxOutKey {
    code: u8,
    txid: FullHash,
    vout: u16,
}

struct TxOutRow {
    key: TxOutKey,
    value: Bytes, // serialized output
}

impl TxOutRow {
    fn new(txid: &FullHash, vout: usize, txout: &TxOut) -> TxOutRow {
        TxOutRow {
            key: TxOutKey {
                code: b'O',
                txid: *txid,
                vout: vout as u16,
            },
            value: serialize(txout),
        }
    }
    fn key(outpoint: &OutPoint) -> Bytes {
        bincode::serialize_little(&TxOutKey {
            code: b'O',
            txid: full_hash(&outpoint.txid[..]),
            vout: outpoint.vout as u16,
        })
        .unwrap()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BlockKey {
    code: u8,
    hash: FullHash,
}

struct BlockRow {
    key: BlockKey,
    value: Bytes, // serialized output
}

impl BlockRow {
    fn new_header(block_entry: &BlockEntry) -> BlockRow {
        BlockRow {
            key: BlockKey {
                code: b'B',
                hash: full_hash(&block_entry.entry.hash()[..]),
            },
            value: serialize(&block_entry.block.header),
        }
    }

    fn new_txids(hash: FullHash, txids: &[Txid]) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'X', hash },
            value: bincode::serialize_little(txids).unwrap(),
        }
    }

    fn new_meta(hash: FullHash, meta: &BlockMeta) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'M', hash },
            value: bincode::serialize_little(meta).unwrap(),
        }
    }

    fn new_tweaks(hash: FullHash, tweaks: &[Vec<u8>]) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'W', hash },
            value: bincode::serialize_little(tweaks).unwrap(),
        }
    }

    fn new_done(hash: FullHash) -> BlockRow {
        BlockRow {
            key: BlockKey { code: b'D', hash },
            value: vec![],
        }
    }

    fn header_filter() -> Bytes {
        b"B".to_vec()
    }

    fn txids_key(hash: FullHash) -> Bytes {
        [b"X", &hash[..]].concat()
    }

    fn meta_key(hash: FullHash) -> Bytes {
        [b"M", &hash[..]].concat()
    }

    fn tweaks_key(hash: FullHash) -> Bytes {
        [b"W", &hash[..]].concat()
    }

    fn done_filter() -> Bytes {
        b"D".to_vec()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }

    fn from_row(row: DBRow) -> Self {
        BlockRow {
            key: bincode::deserialize_little(&row.key).unwrap(),
            value: row.value,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FundingInfo {
    pub txid: FullHash,
    pub vout: u16,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SpendingInfo {
    pub txid: FullHash, // spending transaction
    pub vin: u16,
    pub prev_txid: FullHash, // funding transaction
    pub prev_vout: u16,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TxHistoryInfo {
    Funding(FundingInfo),
    Spending(SpendingInfo),

    #[cfg(feature = "liquid")]
    Issuing(asset::IssuingInfo),
    #[cfg(feature = "liquid")]
    Burning(asset::BurningInfo),
    #[cfg(feature = "liquid")]
    Pegin(peg::PeginInfo),
    #[cfg(feature = "liquid")]
    Pegout(peg::PegoutInfo),
}

impl TxHistoryInfo {
    pub fn get_txid(&self) -> Txid {
        match self {
            TxHistoryInfo::Funding(FundingInfo { txid, .. })
            | TxHistoryInfo::Spending(SpendingInfo { txid, .. }) => deserialize(txid),

            #[cfg(feature = "liquid")]
            TxHistoryInfo::Issuing(asset::IssuingInfo { txid, .. })
            | TxHistoryInfo::Burning(asset::BurningInfo { txid, .. })
            | TxHistoryInfo::Pegin(peg::PeginInfo { txid, .. })
            | TxHistoryInfo::Pegout(peg::PegoutInfo { txid, .. }) => deserialize(txid),
        }
        .expect("cannot parse Txid")
    }
}

#[derive(Serialize, Deserialize)]
pub struct TxHistoryKey {
    pub code: u8,              // H for script history or I for asset history (elements only)
    pub hash: FullHash, // either a scripthash (always on bitcoin) or an asset id (elements only)
    pub confirmed_height: u32, // MUST be serialized as big-endian (for correct scans).
    pub txinfo: TxHistoryInfo,
}

pub struct TxHistoryRow {
    pub key: TxHistoryKey,
}

impl TxHistoryRow {
    fn new(script: &Script, confirmed_height: u32, txinfo: TxHistoryInfo) -> Self {
        let key = TxHistoryKey {
            code: b'H',
            hash: compute_script_hash(&script),
            confirmed_height,
            txinfo,
        };
        TxHistoryRow { key }
    }

    fn filter(code: u8, hash_prefix: &[u8]) -> Bytes {
        [&[code], hash_prefix].concat()
    }

    fn prefix_end(code: u8, hash: &[u8]) -> Bytes {
        bincode::serialize_big(&(code, full_hash(&hash[..]), std::u32::MAX)).unwrap()
    }

    fn prefix_height(code: u8, hash: &[u8], height: u32) -> Bytes {
        bincode::serialize_big(&(code, full_hash(&hash[..]), height)).unwrap()
    }

    pub fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_big(&self.key).unwrap(),
            value: vec![],
        }
    }

    pub fn from_row(row: DBRow) -> Self {
        let key = bincode::deserialize_big(&row.key).expect("failed to deserialize TxHistoryKey");
        TxHistoryRow { key }
    }

    pub fn get_txid(&self) -> Txid {
        self.key.txinfo.get_txid()
    }
    fn get_funded_outpoint(&self) -> OutPoint {
        self.key.txinfo.get_funded_outpoint()
    }
}

impl TxHistoryInfo {
    // for funding rows, returns the funded output.
    // for spending rows, returns the spent previous output.
    pub fn get_funded_outpoint(&self) -> OutPoint {
        match self {
            TxHistoryInfo::Funding(ref info) => OutPoint {
                txid: deserialize(&info.txid).unwrap(),
                vout: info.vout as u32,
            },
            TxHistoryInfo::Spending(ref info) => OutPoint {
                txid: deserialize(&info.prev_txid).unwrap(),
                vout: info.prev_vout as u32,
            },
            #[cfg(feature = "liquid")]
            TxHistoryInfo::Issuing(_)
            | TxHistoryInfo::Burning(_)
            | TxHistoryInfo::Pegin(_)
            | TxHistoryInfo::Pegout(_) => unreachable!(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxEdgeKey {
    code: u8,
    funding_txid: FullHash,
    funding_vout: u16,
    spending_txid: FullHash,
    spending_vin: u16,
}

struct TxEdgeRow {
    key: TxEdgeKey,
}

impl TxEdgeRow {
    fn new(
        funding_txid: FullHash,
        funding_vout: u16,
        spending_txid: FullHash,
        spending_vin: u16,
    ) -> Self {
        let key = TxEdgeKey {
            code: b'S',
            funding_txid,
            funding_vout,
            spending_txid,
            spending_vin,
        };
        TxEdgeRow { key }
    }

    fn filter(outpoint: &OutPoint) -> Bytes {
        // TODO build key without using bincode? [ b"S", &outpoint.txid[..], outpoint.vout?? ].concat()
        bincode::serialize_little(&(b'S', full_hash(&outpoint.txid[..]), outpoint.vout as u16))
            .unwrap()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: vec![],
        }
    }

    fn from_row(row: DBRow) -> Self {
        TxEdgeRow {
            key: bincode::deserialize_little(&row.key).expect("failed to deserialize TxEdgeKey"),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ScriptCacheKey {
    code: u8,
    scripthash: FullHash,
}

struct StatsCacheRow {
    key: ScriptCacheKey,
    value: Bytes,
}

impl StatsCacheRow {
    fn new(scripthash: &[u8], stats: &ScriptStats, blockhash: &BlockHash) -> Self {
        StatsCacheRow {
            key: ScriptCacheKey {
                code: b'A',
                scripthash: full_hash(scripthash),
            },
            value: bincode::serialize_little(&(stats, blockhash)).unwrap(),
        }
    }

    pub fn key(scripthash: &[u8]) -> Bytes {
        [b"A", scripthash].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

type CachedUtxoMap = HashMap<(Txid, u32), (u32, Value)>; // (txid,vout) => (block_height,output_value)

struct UtxoCacheRow {
    key: ScriptCacheKey,
    value: Bytes,
}

impl UtxoCacheRow {
    fn new(scripthash: &[u8], utxos: &UtxoMap, blockhash: &BlockHash) -> Self {
        let utxos_cache = make_utxo_cache(utxos);

        UtxoCacheRow {
            key: ScriptCacheKey {
                code: b'U',
                scripthash: full_hash(scripthash),
            },
            value: bincode::serialize_little(&(utxos_cache, blockhash)).unwrap(),
        }
    }

    pub fn key(scripthash: &[u8]) -> Bytes {
        [b"U", scripthash].concat()
    }

    fn into_row(self) -> DBRow {
        DBRow {
            key: bincode::serialize_little(&self.key).unwrap(),
            value: self.value,
        }
    }
}

// keep utxo cache with just the block height (the hash/timestamp are read later from the headers to reconstruct BlockId)
// and use a (txid,vout) tuple instead of OutPoints (they don't play nicely with bincode serialization)
fn make_utxo_cache(utxos: &UtxoMap) -> CachedUtxoMap {
    utxos
        .iter()
        .map(|(outpoint, (blockid, value))| {
            (
                (outpoint.txid, outpoint.vout),
                (blockid.height as u32, *value),
            )
        })
        .collect()
}

fn from_utxo_cache(utxos_cache: CachedUtxoMap, chain: &ChainQuery) -> UtxoMap {
    utxos_cache
        .into_iter()
        .map(|((txid, vout), (height, value))| {
            let outpoint = OutPoint { txid, vout };
            let blockid = chain
                .blockid_by_height(height as usize)
                .expect("missing blockheader for valid utxo cache entry");
            (outpoint, (blockid, value))
        })
        .collect()
}

// Get the amount value as gets stored in the DB and mempool tracker.
// For bitcoin it is the Amount's inner u64, for elements it is the confidential::Value itself.
pub trait GetAmountVal {
    #[cfg(not(feature = "liquid"))]
    fn amount_value(self) -> u64;
    #[cfg(feature = "liquid")]
    fn amount_value(self) -> confidential::Value;
}

#[cfg(not(feature = "liquid"))]
impl GetAmountVal for bitcoin::Amount {
    fn amount_value(self) -> u64 {
        self.to_sat()
    }
}
#[cfg(feature = "liquid")]
impl GetAmountVal for confidential::Value {
    fn amount_value(self) -> confidential::Value {
        self
    }
}
