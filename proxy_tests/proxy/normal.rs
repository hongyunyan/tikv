// Copyright 2022 TiKV Project Authors. Licensed under Apache-2.0.

use pd_client::PdClient;
use proxy_server::config::{
    address_proxy_config, ensure_no_common_unrecognized_keys, get_last_config,
    setup_default_tikv_config, validate_and_persist_config, TIFLASH_DEFAULT_LISTENING_ADDR,
};
use tikv::config::{TikvConfig, LAST_CONFIG_FILE};

use crate::proxy::*;

mod store {
    use super::*;
    #[test]
    fn test_store_stats() {
        let (mut cluster, pd_client) = new_mock_cluster(0, 1);

        let _ = cluster.run();

        for id in cluster.engines.keys() {
            let engine = cluster.get_tiflash_engine(*id);
            assert_eq!(
                engine.ffi_hub.as_ref().unwrap().get_store_stats().capacity,
                444444
            );
        }

        for id in cluster.engines.keys() {
            cluster.must_send_store_heartbeat(*id);
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
        // let resp = block_on(pd_client.store_heartbeat(Default::default(), None,
        // None)).unwrap();
        for id in cluster.engines.keys() {
            let store_stat = pd_client.get_store_stats(*id).unwrap();
            assert_eq!(store_stat.get_capacity(), 444444);
            assert_eq!(store_stat.get_available(), 333333);
        }
        // The same to mock-engine-store
        cluster.shutdown();
    }
}

mod config {
    use super::*;

    /// Test for double read into both ProxyConfig and TikvConfig.
    #[test]
    fn test_config() {
        // Test double read.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let text = "memory-usage-high-water=0.65\n[server]\nengine-addr=\"1.2.3.4:5\"\n[raftstore]\nsnap-handle-pool-size=4\n[nosense]\nfoo=2\n[rocksdb]\nmax-open-files = 111\nz=1";
        write!(file, "{}", text).unwrap();
        let path = file.path();

        let mut unrecognized_keys = Vec::new();
        let mut config = TikvConfig::from_file(path, Some(&mut unrecognized_keys)).unwrap();
        // Otherwise we have no default addr for TiKv.
        setup_default_tikv_config(&mut config);
        assert_eq!(config.memory_usage_high_water, 0.65);
        assert_eq!(config.rocksdb.max_open_files, 111);
        assert_eq!(config.server.addr, TIFLASH_DEFAULT_LISTENING_ADDR);
        assert_eq!(unrecognized_keys.len(), 3);

        let mut proxy_unrecognized_keys = Vec::new();
        let proxy_config =
            ProxyConfig::from_file(path, Some(&mut proxy_unrecognized_keys)).unwrap();
        assert_eq!(proxy_config.raft_store.snap_handle_pool_size, 4);
        assert_eq!(proxy_config.server.engine_addr, "1.2.3.4:5");
        assert_eq!(proxy_config.memory_usage_high_water, 0.65);
        assert!(proxy_unrecognized_keys.contains(&"nosense".to_string()));
        let v1 = vec!["a.b", "b"]
            .iter()
            .map(|e| String::from(*e))
            .collect::<Vec<String>>();
        let v2 = vec!["a.b", "b.b", "c"]
            .iter()
            .map(|e| String::from(*e))
            .collect::<Vec<String>>();
        let unknown = ensure_no_common_unrecognized_keys(&v1, &v2);
        assert_eq!(unknown.is_err(), true);
        assert_eq!(unknown.unwrap_err(), "a.b, b.b");
        let unknown =
            ensure_no_common_unrecognized_keys(&proxy_unrecognized_keys, &unrecognized_keys);
        assert_eq!(unknown.is_err(), true);
        assert_eq!(unknown.unwrap_err(), "nosense, rocksdb.z");

        // Common config can be persisted.
        // Need run this test with ENGINE_LABEL_VALUE=tiflash, otherwise will fatal
        // exit.
        let _ = std::fs::remove_file(
            PathBuf::from_str(&config.storage.data_dir)
                .unwrap()
                .join(LAST_CONFIG_FILE),
        );
        validate_and_persist_config(&mut config, true);

        // Will not override ProxyConfig
        let proxy_config_new = ProxyConfig::from_file(path, None).unwrap();
        assert_eq!(proxy_config_new.raft_store.snap_handle_pool_size, 4);
    }

    /// Test for basic address_proxy_config.
    #[test]
    fn test_validate_config() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let text = "[raftstore.aaa]\nbbb=2\n[server]\nengine-addr=\"1.2.3.4:5\"\n[raftstore]\nsnap-handle-pool-size=4\nclean-stale-ranges-tick=9999\n[nosense]\nfoo=2\n[rocksdb]\nmax-open-files = 111\nz=1";
        write!(file, "{}", text).unwrap();
        let path = file.path();
        let tmp_store_folder = tempfile::TempDir::new().unwrap();
        let tmp_last_config_path = tmp_store_folder.path().join(LAST_CONFIG_FILE);
        std::fs::copy(path, tmp_last_config_path.as_path()).unwrap();
        get_last_config(tmp_store_folder.path().to_str().unwrap());

        let mut unrecognized_keys: Vec<String> = vec![];
        let mut config = TikvConfig::from_file(path, Some(&mut unrecognized_keys)).unwrap();
        assert_eq!(config.raft_store.clean_stale_ranges_tick, 9999);
        address_proxy_config(&mut config, &ProxyConfig::default());
        let clean_stale_ranges_tick =
            (10_000 / config.raft_store.region_worker_tick_interval.as_millis()) as usize;
        assert_eq!(
            config.raft_store.clean_stale_ranges_tick,
            clean_stale_ranges_tick
        );
    }

    #[test]
    fn test_store_setup() {
        let (mut cluster, pd_client) = new_mock_cluster(0, 3);

        // Add label to cluster
        address_proxy_config(&mut cluster.cfg.tikv, &ProxyConfig::default());

        // Try to start this node, return after persisted some keys.
        let _ = cluster.start();
        let store_id = cluster.engines.keys().last().unwrap();
        let store = pd_client.get_store(*store_id).unwrap();
        println!("store {:?}", store);
        assert!(
            store
                .get_labels()
                .iter()
                .find(|&x| x.key == "engine" && x.value == "tiflash")
                .is_some()
        );
        cluster.shutdown();
    }
}

mod ingest {
    use sst_importer::SstImporter;
    use test_sst_importer::gen_sst_file_with_kvs;

    use super::*;

    pub fn new_ingest_sst_cmd(meta: SstMeta) -> Request {
        let mut cmd = Request::default();
        cmd.set_cmd_type(CmdType::IngestSst);
        cmd.mut_ingest_sst().set_sst(meta);
        cmd
    }

    pub fn create_tmp_importer(cfg: &Config, kv_path: &str) -> (PathBuf, Arc<SstImporter>) {
        let dir = Path::new(kv_path).join("import-sst");
        let importer = {
            Arc::new(
                SstImporter::new(&cfg.import, dir.clone(), None, cfg.storage.api_version())
                    .unwrap(),
            )
        };
        (dir, importer)
    }

    fn make_sst(
        cluster: &Cluster<NodeCluster>,
        region_id: u64,
        region_epoch: RegionEpoch,
        keys: Vec<String>,
    ) -> (PathBuf, SstMeta, PathBuf) {
        let path = cluster.engines.iter().last().unwrap().1.kv.path();
        let (import_dir, importer) = create_tmp_importer(&cluster.cfg, path);

        // Prepare data
        let mut kvs: Vec<(&[u8], &[u8])> = Vec::new();
        let mut keys = keys;
        keys.sort();
        for i in 0..keys.len() {
            kvs.push((keys[i].as_bytes(), b"2"));
        }

        // Make file
        let sst_path = import_dir.join("test.sst");
        let (mut meta, data) = gen_sst_file_with_kvs(&sst_path, &kvs);
        meta.set_region_id(region_id);
        meta.set_region_epoch(region_epoch);
        meta.set_cf_name("default".to_owned());
        let mut file = importer.create(&meta).unwrap();
        file.append(&data).unwrap();
        file.finish().unwrap();

        // copy file to save dir.
        let src = sst_path.clone();
        let dst = file.get_import_path().save.to_str().unwrap();
        let _ = std::fs::copy(src.clone(), dst);

        (file.get_import_path().save.clone(), meta, sst_path)
    }

    #[test]
    fn test_handle_ingest_sst() {
        let (mut cluster, _pd_client) = new_mock_cluster(0, 1);
        let _ = cluster.run();

        let key = "k";
        cluster.must_put(key.as_bytes(), b"v");
        let region = cluster.get_region(key.as_bytes());

        let (file, meta, sst_path) = make_sst(
            &cluster,
            region.get_id(),
            region.get_region_epoch().clone(),
            (0..100).map(|i| format!("k{}", i)).collect::<Vec<_>>(),
        );

        let req = new_ingest_sst_cmd(meta);
        let _ = cluster.request(
            key.as_bytes(),
            vec![req],
            false,
            Duration::from_secs(5),
            true,
        );

        check_key(&cluster, b"k66", b"2", Some(true), Some(true), None);

        assert!(sst_path.as_path().is_file());
        assert!(!file.as_path().is_file());
        std::fs::remove_file(sst_path.as_path()).unwrap();
        cluster.shutdown();
    }

    #[test]
    fn test_invalid_ingest_sst() {
        let (mut cluster, _pd_client) = new_mock_cluster(0, 1);

        let _ = cluster.run();

        let key = "k";
        cluster.must_put(key.as_bytes(), b"v");
        let region = cluster.get_region(key.as_bytes());

        let mut bad_epoch = RegionEpoch::default();
        bad_epoch.set_conf_ver(999);
        bad_epoch.set_version(999);
        let (file, meta, sst_path) = make_sst(
            &cluster,
            region.get_id(),
            bad_epoch,
            (0..100).map(|i| format!("k{}", i)).collect::<Vec<_>>(),
        );

        let req = new_ingest_sst_cmd(meta);
        let _ = cluster.request(
            key.as_bytes(),
            vec![req],
            false,
            Duration::from_secs(5),
            false,
        );
        check_key(&cluster, b"k66", b"2", Some(false), Some(false), None);

        assert!(sst_path.as_path().is_file());
        assert!(!file.as_path().is_file());
        std::fs::remove_file(sst_path.as_path()).unwrap();
        cluster.shutdown();
    }

    #[test]
    fn test_ingest_return_none() {
        let (mut cluster, _pd_client) = new_mock_cluster(0, 1);

        disable_auto_gen_compact_log(&mut cluster);

        let _ = cluster.run();

        cluster.must_put(b"k1", b"v1");
        cluster.must_put(b"k5", b"v5");
        let region = cluster.get_region(b"k1");
        cluster.must_split(&region, b"k5");
        let region1 = cluster.get_region(b"k1");
        let region5 = cluster.get_region(b"k5");
        assert_ne!(region1.get_id(), region5.get_id());

        fail::cfg("on_handle_ingest_sst_return", "return").unwrap();

        let prev_states1 = collect_all_states(&cluster, region1.get_id());
        let prev_states5 = collect_all_states(&cluster, region5.get_id());
        let (file1, meta1, sst_path1) = make_sst(
            &cluster,
            region1.get_id(),
            region1.get_region_epoch().clone(),
            (0..100).map(|i| format!("k1_{}", i)).collect::<Vec<_>>(),
        );
        assert!(sst_path1.as_path().is_file());

        let req = new_ingest_sst_cmd(meta1);
        let _ = cluster.request(b"k1", vec![req], false, Duration::from_secs(5), true);

        let (file5, meta5, _sst_path5) = make_sst(
            &cluster,
            region5.get_id(),
            region5.get_region_epoch().clone(),
            (0..100).map(|i| format!("k5_{}", i)).collect::<Vec<_>>(),
        );
        let req = new_ingest_sst_cmd(meta5);
        let _ = cluster.request(b"k5", vec![req], false, Duration::from_secs(5), true);

        check_key(&cluster, b"k1_66", b"2", Some(true), Some(false), None);
        check_key(&cluster, b"k5_66", b"2", Some(true), Some(false), None);

        let new_states1 = collect_all_states(&cluster, region1.get_id());
        let new_states5 = collect_all_states(&cluster, region5.get_id());
        must_altered_memory_apply_state(&prev_states1, &new_states1);
        must_unaltered_memory_apply_term(&prev_states1, &new_states1);
        must_unaltered_disk_apply_state(&prev_states1, &new_states1);

        must_altered_memory_apply_state(&prev_states5, &new_states5);
        must_unaltered_memory_apply_term(&prev_states5, &new_states5);
        must_unaltered_disk_apply_state(&prev_states5, &new_states5);
        let prev_states1 = new_states1;
        let prev_states5 = new_states5;
        // Not deleted
        assert!(file1.as_path().is_file());
        assert!(file5.as_path().is_file());
        fail::remove("on_handle_ingest_sst_return");

        let (file11, meta11, sst_path11) = make_sst(
            &cluster,
            region1.get_id(),
            region1.get_region_epoch().clone(),
            (200..300).map(|i| format!("k1_{}", i)).collect::<Vec<_>>(),
        );
        assert!(sst_path11.as_path().is_file());

        let req = new_ingest_sst_cmd(meta11);
        let _ = cluster.request(b"k1", vec![req], false, Duration::from_secs(5), true);

        check_key(&cluster, b"k1_222", b"2", Some(true), None, None);
        check_key(&cluster, b"k5_66", b"2", Some(false), None, None);

        let new_states1 = collect_all_states(&cluster, region1.get_id());
        let new_states5 = collect_all_states(&cluster, region5.get_id());
        // Region 1 is persisted.
        must_altered_memory_apply_state(&prev_states1, &new_states1);
        must_unaltered_memory_apply_term(&prev_states1, &new_states1);
        must_altered_disk_apply_state(&prev_states1, &new_states1);
        // Region 5 not persisted yet.
        must_unaltered_disk_apply_state(&prev_states5, &new_states5);
        // file1 and file11 for region 1 is deleted.
        assert!(!file1.as_path().is_file());
        assert!(!file11.as_path().is_file());
        assert!(file5.as_path().is_file());

        // ssp_path1/11/5 share one path.
        std::fs::remove_file(sst_path1.as_path()).unwrap();
        cluster.shutdown();
    }
}

mod restart {
    use super::*;

    #[test]
    fn test_snap_append_restart() {
        let (mut cluster, pd_client) = new_mock_cluster(0, 3);

        disable_auto_gen_compact_log(&mut cluster);
        cluster.cfg.raft_store.max_snapshot_file_raw_size = ReadableSize(u64::MAX);

        // Disable default max peer count check.
        pd_client.disable_default_operator();
        let r1 = cluster.run_conf_change();

        let first_value = vec![0; 10240];
        for i in 0..10 {
            let key = format!("{:03}", i);
            cluster.must_put(key.as_bytes(), &first_value);
        }
        let first_key: &[u8] = b"000";

        let eng_ids = cluster
            .engines
            .iter()
            .map(|e| e.0.to_owned())
            .collect::<Vec<_>>();

        let engine_2 = cluster.get_engine(eng_ids[1]);
        pd_client.must_add_peer(r1, new_peer(eng_ids[1], eng_ids[1]));
        must_get_equal(&engine_2, first_key, first_value.as_slice());

        fail::cfg("apply_pending_snapshot", "return").unwrap();
        tikv_util::info!("engine_3 is {}", eng_ids[2]);
        let engine_3 = cluster.get_engine(eng_ids[2]);
        must_get_none(&engine_3, first_key);
        pd_client.must_add_peer(r1, new_peer(eng_ids[2], eng_ids[2]));

        std::thread::sleep(std::time::Duration::from_millis(1000));
        {
            let (key, value) = (b"k2", b"v2");
            cluster.must_put(key, value);
            check_key(
                &cluster,
                key,
                value,
                Some(true),
                None,
                Some(vec![eng_ids[0], eng_ids[1]]),
            );
            let new_states = maybe_collect_states(&cluster, r1, None);
            // engine_3 has not applied snapshot.
            assert!(new_states.get(&eng_ids[2]).is_none());
            // engine_2 has applied snapshot.
            assert_eq!(
                new_states
                    .get(&eng_ids[1])
                    .unwrap()
                    .in_disk_region_state
                    .get_state(),
                PeerState::Normal
            );
        }

        stop_tiflash_node(&mut cluster, eng_ids[2]);
        restart_tiflash_node(&mut cluster, eng_ids[2]);

        fail::remove("apply_pending_snapshot");

        check_key(
            &cluster,
            first_key,
            &first_value,
            Some(true),
            None,
            Some(vec![eng_ids[2]]),
        );

        cluster.shutdown();
    }

    /// This test is currently not valid, since we can't abort in apply_snap by
    /// failpoint now.
    // #[test]
    fn test_snap_restart() {
        let (mut cluster, pd_client) = new_mock_cluster(0, 3);

        fail::cfg("on_can_apply_snapshot", "return(true)").unwrap();
        disable_auto_gen_compact_log(&mut cluster);
        cluster.cfg.raft_store.max_snapshot_file_raw_size = ReadableSize(u64::MAX);

        // Disable default max peer count check.
        pd_client.disable_default_operator();
        let r1 = cluster.run_conf_change();

        let first_value = vec![0; 10240];
        for i in 0..10 {
            let key = format!("{:03}", i);
            cluster.must_put(key.as_bytes(), &first_value);
        }
        let first_key: &[u8] = b"000";

        let eng_ids = cluster
            .engines
            .iter()
            .map(|e| e.0.to_owned())
            .collect::<Vec<_>>();

        tikv_util::info!("engine_2 is {}", eng_ids[1]);
        // engine 2 will not exec post apply snapshot.
        fail::cfg("on_ob_pre_handle_snapshot", "return").unwrap();
        fail::cfg("on_ob_post_apply_snapshot", "return").unwrap();

        let engine_2 = cluster.get_engine(eng_ids[1]);
        must_get_none(&engine_2, first_key);
        // add peer (engine_2,engine_2) to region 1.
        pd_client.must_add_peer(r1, new_peer(eng_ids[1], eng_ids[1]));

        check_key(&cluster, first_key, &first_value, Some(false), None, None);

        // If we wait here any longer, the snapshot can be applied...
        // So we have to disable this test.
        // std::thread::sleep(std::time::Duration::from_millis(2500));

        stop_tiflash_node(&mut cluster, eng_ids[1]);
        fail::remove("on_ob_pre_handle_snapshot");
        restart_tiflash_node(&mut cluster, eng_ids[1]);

        let (key, value) = (b"k2", b"v2");
        cluster.must_put(key, value);
        // we can get in memory, since snapshot is pre handled, though it is not
        // persisted
        check_key(
            &cluster,
            key,
            value,
            Some(true),
            None,
            Some(vec![eng_ids[1]]),
        );
        // now snapshot must be applied on peer engine_2
        check_key(
            &cluster,
            first_key,
            first_value.as_slice(),
            Some(true),
            None,
            Some(vec![eng_ids[1]]),
        );

        cluster.shutdown();
    }

    #[test]
    fn test_kv_restart() {
        // Test if a empty command can be observed when leadership changes.
        let (mut cluster, _pd_client) = new_mock_cluster(0, 3);

        // Disable AUTO generated compact log.
        disable_auto_gen_compact_log(&mut cluster);

        // We don't handle CompactLog at all.
        fail::cfg("try_flush_data", "return(0)").unwrap();
        let _ = cluster.run();

        cluster.must_put(b"k", b"v");
        let region = cluster.get_region(b"k");
        let region_id = region.get_id();
        for i in 0..10 {
            let k = format!("k{}", i);
            let v = format!("v{}", i);
            cluster.must_put(k.as_bytes(), v.as_bytes());
        }
        let prev_state = collect_all_states(&cluster, region_id);
        let (compact_index, compact_term) = get_valid_compact_index(&prev_state);
        let compact_log = test_raftstore::new_compact_log_request(compact_index, compact_term);
        let req =
            test_raftstore::new_admin_request(region_id, region.get_region_epoch(), compact_log);
        fail::cfg("try_flush_data", "return(1)").unwrap();
        let _ = cluster
            .call_command_on_leader(req, Duration::from_secs(3))
            .unwrap();

        let eng_ids = cluster
            .engines
            .iter()
            .map(|e| e.0.to_owned())
            .collect::<Vec<_>>();

        for i in 0..10 {
            let k = format!("k{}", i);
            let v = format!("v{}", i);
            // Whatever already persisted or not, we won't loss data.
            check_key(
                &cluster,
                k.as_bytes(),
                v.as_bytes(),
                Some(true),
                Some(true),
                Some(vec![eng_ids[0]]),
            );
        }

        for i in 10..20 {
            let k = format!("k{}", i);
            let v = format!("v{}", i);
            cluster.must_put(k.as_bytes(), v.as_bytes());
        }

        for i in 10..20 {
            let k = format!("k{}", i);
            let v = format!("v{}", i);
            // Whatever already persisted or not, we won't loss data.
            check_key(
                &cluster,
                k.as_bytes(),
                v.as_bytes(),
                Some(true),
                Some(false),
                Some(vec![eng_ids[0]]),
            );
        }

        stop_tiflash_node(&mut cluster, eng_ids[0]);
        restart_tiflash_node(&mut cluster, eng_ids[0]);

        std::thread::sleep(std::time::Duration::from_millis(2000));

        for i in 0..20 {
            let k = format!("k{}", i);
            let v = format!("v{}", i);
            // Whatever already persisted or not, we won't loss data.
            check_key(
                &cluster,
                k.as_bytes(),
                v.as_bytes(),
                Some(true),
                None,
                Some(vec![eng_ids[0]]),
            );
        }

        fail::remove("try_flush_data");
        cluster.shutdown();
    }
}

mod persist {
    use super::*;

    #[test]
    fn test_persist_when_finish() {
        let (mut cluster, _pd_client) = new_mock_cluster(0, 3);
        disable_auto_gen_compact_log(&mut cluster);

        cluster.run();
        cluster.must_put(b"k0", b"v0");
        check_key(&cluster, b"k0", b"v0", Some(true), Some(false), None);
        let region_id = cluster.get_region(b"k0").get_id();

        let prev_states = collect_all_states(&cluster, region_id);
        cluster.must_put(b"k1", b"v1");
        check_key(&cluster, b"k1", b"v1", Some(true), Some(false), None);
        let new_states = collect_all_states(&cluster, region_id);
        must_altered_memory_apply_index(&prev_states, &new_states, 1);
        must_altered_disk_apply_index(&prev_states, &new_states, 0);

        fail::cfg("on_pre_persist_with_finish", "return").unwrap();
        cluster.must_put(b"k2", b"v2");
        // Because we flush when batch ends.
        check_key(&cluster, b"k2", b"v2", Some(true), Some(false), None);

        // TODO(tiflash) wait `write_apply_state` in raftstore.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let prev_states = collect_all_states(&cluster, region_id);
        cluster.must_put(b"k3", b"v3");
        // Because we flush when batch ends.
        check_key(&cluster, b"k3", b"v3", Some(true), Some(false), None);

        // TODO(tiflash) wait `write_apply_state` in raftstore.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let new_states = collect_all_states(&cluster, region_id);
        must_apply_index_advanced_diff(&prev_states, &new_states, 0);
        fail::remove("on_pre_persist_with_finish");
    }

    #[test]
    fn test_persist_when_merge() {
        let (mut cluster, pd_client) = new_mock_cluster_snap(0, 3);
        assert_eq!(cluster.cfg.proxy_cfg.raft_store.snap_handle_pool_size, 2);

        // disable_auto_gen_compact_log(&mut cluster);
        cluster.cfg.raft_store.right_derive_when_split = false;

        cluster.run();

        cluster.must_put(b"k1", b"v1");
        cluster.must_put(b"k3", b"v3");

        check_key(&cluster, b"k1", b"v1", Some(true), None, None);
        check_key(&cluster, b"k3", b"v3", Some(true), None, None);

        let r1 = cluster.get_region(b"k1");
        cluster.must_split(&r1, b"k2");
        let r3 = cluster.get_region(b"k3");

        std::thread::sleep(std::time::Duration::from_millis(1000));
        let prev_states = collect_all_states(&cluster, r3.get_id());

        info!("start merge"; "from" => r1.get_id(), "to" => r3.get_id());
        pd_client.must_merge(r1.get_id(), r3.get_id());

        // TODO(tiflash) wait `write_apply_state` in raftstore.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let r3_new = cluster.get_region(b"k3");
        assert_eq!(r3_new.get_id(), r3.get_id());
        let new_states = collect_all_states(&cluster, r3_new.get_id());
        // index 6 empty command
        // index 7 CommitMerge
        must_altered_disk_apply_index(&prev_states, &new_states, 2);
    }
}
