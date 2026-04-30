# Graph Report - phantom  (2026-04-30)

## Corpus Check
- Large corpus: 312 files · ~506,305 words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder, or use --no-semantic to run AST-only.

## Summary
- 7477 nodes · 23048 edges · 107 communities detected
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 398 edges (avg confidence: 0.5)
- Token cost: 0 input · 0 output


## Crate Dependency Map

| Tier | Crates | Dependencies |
|------|--------|-------------|
| 0 | `phantom-audio` | (none) |
| 0 | `phantom-bundles` | (none) |
| 0 | `phantom-context` | (none) |
| 0 | `phantom-dag` | (none) |
| 0 | `phantom-embeddings` | (none) |
| 0 | `phantom-mcp` | (none) |
| 0 | `phantom-memory` | (none) |
| 0 | `phantom-net` | (none) |
| 0 | `phantom-plugins` | (none) |
| 0 | `phantom-protocol` | (none) |
| 0 | `phantom-relay` | (none) |
| 0 | `phantom-renderer` | (none) |
| 0 | `phantom-scene` | (none) |
| 0 | `phantom-semantic` | (none) |
| 0 | `phantom-stt` | (none) |
| 0 | `phantom-terminal` | (none) |
| 0 | `phantom-vision` | (none) |
| 0 | `phantom-voice` | (none) |
| 1 | `phantom-adapter` | phantom-protocol |
| 1 | `phantom-agents` | phantom-memory, phantom-protocol, phantom-semantic |
| 1 | `phantom-bundle-store` | phantom-bundle-store, phantom-bundles, phantom-embeddings, phantom-vision |
| 1 | `phantom-history` | phantom-semantic |
| 1 | `phantom-nlp` | phantom-context |
| 1 | `phantom-recall` | phantom-bundles |
| 1 | `phantom-supervisor` | phantom-protocol |
| 2 | `phantom-brain` | phantom-adapter, phantom-agents, phantom-context, phantom-history, phantom-memory, phantom-semantic |
| 2 | `phantom-session` | phantom-agents, phantom-context |
| 2 | `phantom-ui` | phantom-adapter, phantom-renderer |
| 3 | `phantom-app` | phantom-adapter, phantom-agents, phantom-brain, phantom-bundle-store, phantom-bundle-store, phantom-bundles, phantom-context, phantom-context, phantom-dag, phantom-embeddings, phantom-history, phantom-mcp, phantom-memory, phantom-nlp, phantom-nlp, phantom-plugins, phantom-protocol, phantom-renderer, phantom-scene, phantom-semantic, phantom-session, phantom-stt, phantom-stt, phantom-terminal, phantom-ui, phantom-ui, phantom-vision |
| 4 | `phantom` | phantom-agents, phantom-app, phantom-brain, phantom-context, phantom-history, phantom-memory, phantom-nlp, phantom-protocol, phantom-renderer, phantom-semantic, phantom-terminal, phantom-ui |


## Cross-Crate Coupling (most imported types)

| Entity | Imported by N files |
|--------|--------------------|
| `AgentTask` | 15 |
| `QuadInstance` | 13 |
| `ProjectContext` | 11 |
| `MemoryStore` | 9 |
| `Disposition` | 6 |
| `Bundle` | 6 |
| `FrameRef` | 6 |
| `TranscriptWord` | 6 |
| `Rect` | 5 |
| `EventLog` | 5 |
| `ParsedOutput` | 5 |
| `Event` | 4 |
| `SceneTree` | 4 |
| `AgentRole` | 4 |
| `CapabilityClass` | 4 |


## Test Coverage Summary

- 3911 production entities, 3536 test entities
- Test nodes are tagged `is_test: true` in graph.json but excluded from community detection

## God Nodes (most connected - your core abstractions)
1. `AppCoordinator` - 46 edges
2. `Agent` - 39 edges
3. `BootSequence` - 35 edges
4. `PhantomTerminal` - 31 edges
5. `TerminalAdapter` - 29 edges
6. `LayoutEngine` - 22 edges
7. `SemanticParser` - 21 edges
8. `Supervisor` - 21 edges
9. `SceneTree` - 21 edges
10. `AgentJournal` - 21 edges

## Surprising Connections (you probably didn't know these)
- `crate:phantom` --contains_entity--> `parses_shell_path_from_output()`  [EXTRACTED]
  crates/phantom/Cargo.toml → crates/phantom/src/path_resolver.rs
- `crate:phantom` --contains_entity--> `merges_shell_path_before_current()`  [EXTRACTED]
  crates/phantom/Cargo.toml → crates/phantom/src/path_resolver.rs
- `crate:phantom` --contains_entity--> `timeout_kills_hung_process()`  [EXTRACTED]
  crates/phantom/Cargo.toml → crates/phantom/src/path_resolver.rs
- `crate:phantom` --contains_entity--> `corrupt_output_without_slash_is_rejected()`  [EXTRACTED]
  crates/phantom/Cargo.toml → crates/phantom/src/path_resolver.rs
- `crate:phantom` --contains_entity--> `empty_shell_path_is_handled()`  [EXTRACTED]
  crates/phantom/Cargo.toml → crates/phantom/src/path_resolver.rs

## Communities

### Community 0 - "AgentTask (phantom-agents, phantom-session)"
Cohesion: 0.01
Nodes (321): AppActionHandler, AgentMessage, AgentSpawnOpts, AgentTask, AgentOutputCapture, AgentRecord, CapturedAgent, default_data_dir() (+313 more)

### Community 1 - "TerminalAdapter (phantom-app, phantom-scene)"
Cohesion: 0.01
Nodes (303): AppAdapter, AppCore, BusParticipant, Commandable, CursorData, CursorShape, GridData, InputHandler (+295 more)

### Community 2 - "InspectorAdapter (phantom-ui)"
Cohesion: 0.01
Nodes (402): ShaderOverrides, CursorBlink, Divider, Orientation, FocusRing, InputBar, InputKey, AgentRow (+394 more)

### Community 3 - "MemoryStore (phantom-brain)"
Cohesion: 0.01
Nodes (266): action_name(), brain_loop(), brain_supervised(), BrainConfig, BrainHandle, diagnose_build_failure(), enhance_with_claude(), enhance_with_investigation() (+258 more)

### Community 4 - "PluginRegistry (phantom-renderer)"
Cohesion: 0.02
Nodes (138): App, AppState, build_ticket_dispatcher(), FloatInteraction, NlpTranslateResult, open_bundle_store(), ResizeEdge, SuggestionOverlay (+130 more)

### Community 5 - "EventLog (phantom-agents)"
Cohesion: 0.02
Nodes (247): check_capability(), class_for(), agent_is_quarantined(), collect_source_chain(), quarantine_registry_blocks(), source_agent_id_from_envelope(), taint_from_source_chain(), append_speak_to_log() (+239 more)

### Community 6 - "Bundle (phantom-bundle-store, phantom-bundles)"
Cohesion: 0.02
Nodes (188): AssemblerError, BundleAssembler, Embedding, Modality, Modality, InMemoryVectorIndex, ModalityTable, VectorHit (+180 more)

### Community 7 - "PhantomTerminal (phantom-terminal, phantom-app)"
Cohesion: 0.02
Nodes (122): ContextMenu, MenuAction, MenuItem, LayoutEngine, PaneId, Rect, App, pixel_to_cell() (+114 more)

### Community 8 - "AgentJournal (phantom-memory)"
Cohesion: 0.03
Nodes (105): McpError, HandoffEntry, HandoffError, HandoffLog, new_entry(), now_unix_ms(), AgentJournal, entry_from_envelope() (+97 more)

### Community 9 - "HeadlessActionHandler (phantom-nlp, phantom-agents)"
Cohesion: 0.01
Nodes (251): AgentStatus, AgentCommand, execute_agent_command(), format_agent_detail(), format_agent_list(), format_capability(), format_duration(), format_model() (+243 more)

### Community 10 - "AgentManager (phantom-agents)"
Cohesion: 0.01
Nodes (230): ApiEvent, ApiHandle, build_messages_with_ids(), build_request_body(), build_tools(), ClaudeConfig, parse_response(), send_message() (+222 more)

### Community 11 - "OpenAiBackend (phantom-stt)"
Cohesion: 0.03
Nodes (124): MockStream, stream(), MockStream, stream(), bincode_config(), CaptureEvent, decode_event(), encode_event() (+116 more)

### Community 12 - "MockRuntime (phantom-plugins)"
Cohesion: 0.03
Nodes (125): api_inspector(), docker_dashboard(), get_official(), git_enhanced(), github_notifications(), official_plugins(), spotify_controls(), hook_type_key() (+117 more)

### Community 13 - "SemanticParser (phantom-semantic, phantom-agents)"
Cohesion: 0.03
Nodes (120): ErrorHighlighter, HighlightRegion, HighlightStyle, SourceReference, SemanticParser, ScoredAction, UtilityScorer, append_content_summary() (+112 more)

### Community 14 - "PersistentSkillRegistry (phantom-brain, phantom-agents)"
Cohesion: 0.03
Nodes (76): AuditOutcome, AuditWriter, emit_tool_call(), hash_args(), init(), now_ts(), defender_spawn_rule(), AgentPane (+68 more)

### Community 15 - "AgentRuntime (phantom-agents, phantom-app)"
Cohesion: 0.04
Nodes (67): build_error_analysis_prompt(), ContentBlock, generate(), investigate(), is_available(), Message, MessagesRequest, MessagesResponse (+59 more)

### Community 16 - "KeybindRegistry (phantom-ui, phantom-app)"
Cohesion: 0.03
Nodes (85): ImageHandler, DecodedImage, ImageInstance, ImageManager, ImagePlacement, Uniforms, App, chrono_time_string() (+77 more)

### Community 17 - "Screenshot (phantom-vision)"
Cohesion: 0.05
Nodes (81): Analysis, ChatRequest, ChatResponse, Choice, ContentPart, ImageUrl, Message, OpenAiVisionBackend (+73 more)

### Community 18 - "Supervisor (phantom-supervisor, phantom-app)"
Cohesion: 0.08
Nodes (36): install_signal_handlers(), main(), find_swift_runtime_path(), inject_dyld_fallback(), print_banner(), Supervisor, delete_pid_file(), pid_file_path() (+28 more)

### Community 19 - "TextRenderer (phantom-renderer)"
Cohesion: 0.05
Nodes (28): GlyphAtlas, GlyphEntry, GridCell, GridRenderData, is_default_bg(), Uniforms, GlitchCell, KeystrokeFx (+20 more)

### Community 20 - "AppCoordinator (phantom-app)"
Cohesion: 0.06
Nodes (36): AppCoordinator, register_mock(), test_register_adapter_assigns_unique_id(), test_register_adapter_transitions_to_running(), test_remove_adapter_transitions_to_dead(), test_update_all_calls_adapter_update(), test_render_all_returns_outputs_for_visual_adapters(), test_route_input_to_focused_adapter() (+28 more)

### Community 21 - "BootSequence (phantom-app)"
Cohesion: 0.07
Nodes (37): BootPhase, BootSequence, BootTextLine, build_syscheck_text(), lerp_color(), LineStyle, noise_char_from_hash(), noise_hash() (+29 more)

### Community 22 - "CodeDag (phantom-dag, phantom-app)"
Cohesion: 0.08
Nodes (43): NodeKind, DagViewerState, DagEdge, EdgeKind, CodeDag, DagNode, build_overlay(), extract_crate_names() (+35 more)

### Community 23 - "Agent (phantom-agents)"
Cohesion: 0.04
Nodes (55): Agent, truncate(), push_message_appends(), log_appends_output(), complete_success_sets_done(), complete_failure_sets_failed(), elapsed_returns_duration(), system_prompt_fix_error_includes_summary() (+47 more)

### Community 24 - "RelayClient (phantom-net)"
Cohesion: 0.1
Nodes (29): NonceCounter, PeerId, RelayClient, Envelope, canonical_bytes(), deserialize(), Engine, serialize() (+21 more)

### Community 25 - "InterventionEngine (phantom-brain)"
Cohesion: 0.06
Nodes (50): EnvSignal, EnvState, InterventionDecision, InterventionEngine, InterventionKind, is_build_command(), is_test_command(), NeedEstimate (+42 more)

### Community 26 - "ClaudeBackend (phantom-brain)"
Cohesion: 0.05
Nodes (30): ChatBackend, ChatOptions, ChatResponse, ClaudeBackend, Message, OllamaBackend, OpenAiBackend, OpenAiChoice (+22 more)

### Community 27 - "GhTicketDispatcher (phantom-agents)"
Cohesion: 0.06
Nodes (39): capability_matches_label(), ClaimedSet, DispatcherTool, DispatcherToolContext, gh_issue_to_ticket(), GhIssue, GhIssueSource, GhLabel (+31 more)

### Community 28 - "TaskLedger (phantom-brain)"
Cohesion: 0.05
Nodes (40): Orchestrator, PlanStep, TaskLedger, add_and_query_facts(), verify_fact_promotes_guess(), set_plan_resets_stall(), replan_archives_old_plan(), next_pending_step_skips_done() (+32 more)

### Community 29 - "PhantomConfig (phantom, phantom-app)"
Cohesion: 0.09
Nodes (18): config_path(), PhantomConfig, install_signal_handlers(), main(), GpuContext, App, append(), append_hex() (+10 more)

### Community 30 - "Community 30"
Cohesion: 0.09
Nodes (36): add_crate_summary_nodes(), assign_excluded_nodes(), build_clustering_subgraph(), build_directed_graph(), _build_test_line_ranges(), BuildArtifacts, enhance_report(), export_graph() (+28 more)

### Community 31 - "GoalPursuit (phantom-brain)"
Cohesion: 0.09
Nodes (24): BrainTask, build_prioritization_prompt(), build_reflection_prompt(), build_task_creation_prompt(), GoalPursuit, inject_reflections(), load_reflexion(), parse_task_list() (+16 more)

### Community 32 - "AgentSnapshot (phantom-session)"
Cohesion: 0.08
Nodes (40): AgentSnapshot, AgentStateFile, AgentStatePersister, partial_restore(), RestoreOutcome, SavedMessage, ToolCall, free_agent() (+32 more)

### Community 33 - "DebugDrawManager (phantom-renderer)"
Cohesion: 0.13
Nodes (12): DebugDrawManager, DebugPrimitive, DrawOptions, QueuedPrimitive, Vec2, disabled_manager_ignores_adds(), enabled_manager_queues_primitives(), flush_returns_all_and_decays() (+4 more)

### Community 34 - "SysmonHandle (phantom-app)"
Cohesion: 0.1
Nodes (29): build_monitor_lines(), build_resource_bar(), DiskIoCounters, extract_temp(), format_throughput(), NetCounters, num_cpus_cached(), read_battery() (+21 more)

### Community 35 - "Attention (phantom-adapter, phantom-brain)"
Cohesion: 0.06
Nodes (52): Attention, PaneSnapshot, AdapterEvent, AdapterId, AdapterIdGen, EventStream, adapter_id_new_stores_raw_value(), adapter_id_equality_by_value() (+44 more)

### Community 36 - "HistoryStore (phantom-history)"
Cohesion: 0.14
Nodes (30): HistoryEntryBuilder, default_data_dir(), HistoryStore, lock_path_for(), CommandType, temp_store(), entry(), entry_with_exit() (+22 more)

### Community 37 - "MockVoiceSynth (phantom-voice)"
Cohesion: 0.11
Nodes (32): CachingVoiceSynth, CachingVoiceSynth<B>, MockVoiceSynth, OpenAiVoice, single_chunk_stream(), SingleChunkStream, SynthAudioChunk, VoiceError (+24 more)

### Community 38 - "ProviderCatalog (phantom-brain)"
Cohesion: 0.09
Nodes (26): ProviderCatalog, ProviderProfile, profile_new_stores_fields(), profile_auto_appends_default_model_if_missing(), profile_no_duplicate_when_default_already_present(), profile_supports_model_true(), profile_supports_model_false_for_unknown(), empty_catalog_has_no_profiles() (+18 more)

### Community 39 - "SessionManager (phantom-session)"
Cohesion: 0.09
Nodes (52): is_session_restore(), is_session_restore_with_env(), PaneState, project_hash(), SavedShaderParams, session_dir_path(), SessionManager, SessionState (+44 more)

### Community 40 - "InMemoryStore (phantom-embeddings)"
Cohesion: 0.18
Nodes (25): EmbeddingRecord, EmbeddingStore, InMemoryStore, QueryFilter, QueryHit, record_matches(), StoreError, uuid() (+17 more)

### Community 41 - "QuarantineRegistry (phantom-agents)"
Cohesion: 0.13
Nodes (20): AgentRuntime, AgentSlot, AutoQuarantinePolicy, QuarantineRegistry, QuarantineState, n_minus_one_taints_do_not_quarantine(), n_taints_quarantine_agent(), clean_taint_resets_consecutive_counter() (+12 more)

### Community 42 - "BlockHandle (phantom-memory)"
Cohesion: 0.14
Nodes (19): Block, BlockError, BlockHandle, BlockKey, MemoryStore, Permission, key(), attach_same_key_returns_shared_block() (+11 more)

### Community 43 - "VectorQuery (phantom-recall)"
Cohesion: 0.18
Nodes (28): CorpusEntry, cosine(), InMemoryRecallEngine, mock_embed(), normalise(), normalise_dims(), RecallEngine, RecallResult (+20 more)

### Community 44 - "AppRegistry (phantom-adapter)"
Cohesion: 0.14
Nodes (2): AppRegistry, RegisteredApp

### Community 45 - "SelfTestRunner (phantom-app)"
Cohesion: 0.15
Nodes (12): build_test_suite(), check_result_detailed(), cleanup_after_test(), describe_check(), execute_action(), Failure, HealStage, Phase (+4 more)

### Community 46 - "ResourceManager (phantom-app)"
Cohesion: 0.13
Nodes (17): LoadStatus, Resource, ResourceEntry, ResourceHandle, ResourceId, ResourceManager, TestResource, make_test_resource() (+9 more)

### Community 47 - "SettingsPanel (phantom-app)"
Cohesion: 0.12
Nodes (6): CurrentValues, SettingsItem, SettingsKind, SettingsPanel, SettingsSection, SettingsSnapshot

### Community 48 - "DecompositionStep (phantom-brain)"
Cohesion: 0.14
Nodes (14): DecompositionResult, DecompositionStep, GoalDecomposer, Orchestrator, steps_are_in_priority_order(), cost_estimation_is_sum_of_step_costs(), errors_detected_bumps_fix_priority(), unknown_goal_falls_back_to_investigate() (+6 more)

### Community 49 - "McpClient (phantom-mcp)"
Cohesion: 0.14
Nodes (29): build_call_tool_request(), build_initialize_request(), build_list_tools_request(), McpClient, next_builder_id(), build_initialize_produces_valid_request(), build_list_tools_request_correct_method(), build_call_tool_request_correct_shape() (+21 more)

### Community 50 - "CaptureSession (phantom-bundles)"
Cohesion: 0.15
Nodes (17): CaptureFrame, CaptureSession, TranscriptSegment, frame(), segment(), fresh_id(), finalize_on_empty_session_returns_no_frames_error(), finalize_transcript_only_session_returns_no_frames_error() (+9 more)

### Community 51 - "Console (phantom-app)"
Cohesion: 0.14
Nodes (16): Console, ConsoleLine, console_with_3_commands(), history_up_once_returns_last_command(), history_up_twice_returns_second_command(), history_up_three_times_returns_first_command(), history_up_past_oldest_stays_at_first_no_panic(), history_up_then_down_restores_input() (+8 more)

### Community 52 - "AgentAdapter (phantom-app)"
Cohesion: 0.1
Nodes (14): AgentAdapter, test_render_produces_text_segments(), test_handle_input_accepts_printable_chars(), test_accept_command_unknown_returns_error(), test_is_alive_false_after_dismiss(), agent_adapter_render_stays_within_rect(), scroll_command_up_increments_offset(), scroll_command_down_clamps_at_zero() (+6 more)

### Community 53 - "FailureStore (phantom-agents)"
Cohesion: 0.16
Nodes (16): FailureRecord, FailureStore, free_form_task(), make_record(), make_record_with_tool(), push_and_retrieve_single_record(), store_caps_at_100_records_evicting_oldest(), by_agent_returns_only_matching_records() (+8 more)

### Community 54 - "JobPool (phantom-app)"
Cohesion: 0.13
Nodes (18): JobContext, JobEnvelope, JobHandle, JobId, JobPayload, JobPool, JobPriority, JobResult (+10 more)

### Community 55 - "PhantomMcpServer (phantom-mcp)"
Cohesion: 0.14
Nodes (19): builtin_resources(), builtin_tools(), PhantomMcpServer, ResourceReadParams, ServerInfo, ToolCallParams, server(), initialize_returns_server_info() (+11 more)

### Community 56 - "PhantomModifiers (phantom-terminal)"
Cohesion: 0.05
Nodes (57): encode_arrow(), encode_char(), encode_function_key(), encode_key(), encode_mouse_motion_sgr(), encode_mouse_sgr(), encode_paste(), encode_tilde() (+49 more)

### Community 57 - "App (phantom-app)"
Cohesion: 0.25
Nodes (1): App

### Community 58 - "AgentPane (phantom-app)"
Cohesion: 0.1
Nodes (1): AgentPane

### Community 59 - "Theme (phantom-ui)"
Cohesion: 0.17
Nodes (28): amber(), blood(), builtin_by_name(), hex(), hexa(), ice(), phosphor(), pipboy() (+20 more)

### Community 60 - "Framework (phantom-context)"
Cohesion: 0.12
Nodes (44): detect_commands(), detect_elixir_framework(), detect_framework(), detect_java_framework(), detect_node_framework(), detect_package_manager(), detect_project(), detect_python_framework() (+36 more)

### Community 61 - "PhantomLogger (phantom-app)"
Cohesion: 0.18
Nodes (12): channel_for_target(), chrono_free_timestamp(), install(), install_panic_hook(), level_to_verbosity(), PhantomLogger, channel_for_known_targets(), channel_for_unknown_target_passes() (+4 more)

### Community 62 - "Router (phantom-relay)"
Cohesion: 0.22
Nodes (18): main(), Config, parse_config_flag(), Router, spawn_relay(), handshake(), recv_json(), make_send() (+10 more)

### Community 63 - "MacOsSckCapture (phantom-audio)"
Cohesion: 0.21
Nodes (10): build_and_start_stream(), MacOsSckCapture, map_sc_error(), map_sc_error_with_context(), sample_buffer_to_audio_frame(), SckAudioStream, SckStreamGuard, enumerate_succeeds_or_permission_denied() (+2 more)

### Community 64 - "MonitorAdapter (phantom-app)"
Cohesion: 0.15
Nodes (1): MonitorAdapter

### Community 65 - "AgentPaneStyle (phantom-agents)"
Cohesion: 0.09
Nodes (34): agent_header(), agent_output_lines(), AgentPaneStyle, animated_border_color(), truncate(), style_working_has_nonzero_pulse(), style_waiting_for_tool_uses_working_style(), style_queued_has_no_pulse() (+26 more)

### Community 66 - "SandboxError (phantom-agents)"
Cohesion: 0.14
Nodes (25): CommandOutput, execute_sandboxed(), run_bare(), run_permissive(), run_strict(), run_strict_linux(), run_strict_macos(), SandboxError (+17 more)

### Community 67 - "OpenAiEmbeddingBackend (phantom-embeddings)"
Cohesion: 0.16
Nodes (12): EmbeddingData, EmbeddingsRequest, EmbeddingsResponse, OpenAiEmbeddingBackend, with_env_var(), from_env_returns_not_configured_when_missing(), from_env_succeeds_with_key(), with_small_uses_small_model_and_1536_dim() (+4 more)

### Community 68 - "ReconcilerState (phantom-brain)"
Cohesion: 0.15
Nodes (20): connection_state_for_reason(), ReconcilerState, stall_detection_emits_flatline_on_exhausted_retries(), heartbeat_flatline_carries_reason_and_matching_id(), heartbeat_stall_requeues_before_final_flatline(), reset_clears_active_dispatches(), on_agent_complete_routes_by_spawn_tag_not_by_manager_id(), on_agent_complete_ignores_events_without_spawn_tag() (+12 more)

### Community 69 - "Panel (phantom-ui)"
Cohesion: 0.23
Nodes (4): Panel, set_title_changes_render_output(), removing_title_collapses_to_untitled_output(), degenerate_rect_body_clamped_to_zero()

### Community 70 - "McpToolRegistry (phantom-mcp)"
Cohesion: 0.15
Nodes (18): McpToolRegistry, McpToolRoute, ToolProvenance, register_test_server(), register_two_servers_non_overlapping_tools(), resolve_routes_to_correct_server(), resolve_tool_unknown_returns_none(), invoke_known_tool_returns_ok() (+10 more)

### Community 71 - "PhantomSettings (phantom-app)"
Cohesion: 0.22
Nodes (7): CrtSettings, PhantomSettings, ScrollSettings, default_settings_round_trip(), load_from_nonexistent_returns_default(), save_and_reload(), partial_toml_fills_defaults()

### Community 72 - "Heartbeat (phantom-net)"
Cohesion: 0.19
Nodes (11): backoff_delay(), Heartbeat, HeartbeatAction, HeartbeatState, first_poll_requests_ping(), after_ping_waits_for_pong(), pong_resets_to_idle(), timeout_triggers_reconnect() (+3 more)

### Community 73 - "SceneNode (phantom-scene)"
Cohesion: 0.18
Nodes (5): NodeKind, RenderLayer, SceneNode, Transform, WorldTransform

### Community 74 - "TabStrip (phantom-ui)"
Cohesion: 0.22
Nodes (5): TabStrip, click_on_empty_strip_is_no_op(), click_does_not_change_other_tab_badges(), multiple_badges_independently_reported(), on_select_callback_receives_correct_index_on_click()

### Community 75 - "App (phantom-app)"
Cohesion: 0.33
Nodes (1): App

### Community 76 - "NotificationCenter (phantom-app)"
Cohesion: 0.2
Nodes (6): Banner, NotificationCenter, Severity, triggers_banner_at_threshold(), does_not_trigger_below_threshold(), prunes_old_timestamps_outside_window()

### Community 77 - "LayoutBox (phantom-ui)"
Cohesion: 0.15
Nodes (25): LayoutBox, approx_eq(), parent(), layout_box_is_copy_clone_debug(), reserve_top_carves_strip_and_remainder(), reserve_top_clamps_when_exceeds_parent(), reserve_top_zero_height_is_noop(), reserve_bottom_carves_strip_and_remainder() (+17 more)

### Community 78 - "NotificationBanner (phantom-ui)"
Cohesion: 0.27
Nodes (5): NotificationBanner, visible_emits_background_and_stripe(), severity_drives_accent_color(), text_color_matches_severity_token(), clear_hides_banner()

### Community 79 - "ShutdownGuard (phantom-app)"
Cohesion: 0.18
Nodes (11): BootOrder, ShutdownGuard, SubsystemTier, tiers_are_ordered(), shutdown_order_is_reversed(), foundation_tier_is_first(), all_tiers_have_subsystems(), shutdown_guard_is_idempotent() (+3 more)

### Community 80 - "SelectionRect (phantom-ui)"
Cohesion: 0.15
Nodes (25): normalize(), PixelRect, SelectionMode, SelectionRect, ctx(), sel(), forward_order_unchanged(), reversed_rows_flips_start_and_end() (+17 more)

### Community 81 - "JsonRpcError (phantom-mcp)"
Cohesion: 0.16
Nodes (17): create_error(), create_request(), create_response(), JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpResource, McpTool (+9 more)

### Community 82 - "SupervisorClient (phantom-app)"
Cohesion: 0.32
Nodes (1): SupervisorClient

### Community 83 - "App (phantom-app)"
Cohesion: 0.57
Nodes (1): App

### Community 84 - "WsTransport (phantom-net)"
Cohesion: 0.29
Nodes (1): WsTransport

### Community 85 - "GenerateOptions (phantom-brain)"
Cohesion: 0.27
Nodes (9): build_error_triage_prompt(), generate(), GenerateOptions, GenerateRequest, GenerateResponse, is_available(), build_error_triage_prompt_formats_correctly(), build_error_triage_prompt_caps_at_3_errors() (+1 more)

### Community 86 - "AppState (phantom-adapter)"
Cohesion: 0.33
Nodes (1): AppState

### Community 87 - "Session (phantom-relay)"
Cohesion: 0.33
Nodes (2): Session, SessionHandle

### Community 88 - "HandshakeAck (phantom-relay)"
Cohesion: 0.47
Nodes (5): handle_connection(), HandshakeAck, IdentityProof, run(), run_with_listener()

### Community 89 - "PeerId (phantom-relay)"
Cohesion: 0.33
Nodes (4): Envelope, ClientMessage, PeerId, RelayMessage

### Community 90 - "SequenceClock (phantom-memory)"
Cohesion: 0.42
Nodes (4): SequenceClock, clock_starts_at_zero(), clock_next_increments_monotonically(), clock_is_thread_safe()

### Community 91 - "UnionFind (phantom-dag)"
Cohesion: 0.53
Nodes (4): UnionFind, singleton_is_own_root(), union_merges_components(), disconnected_sets_have_different_roots()

### Community 92 - "GlyphClipRect (phantom-renderer)"
Cohesion: 0.33
Nodes (1): GlyphClipRect

### Community 93 - "ScreenshotMetadata (phantom-renderer)"
Cohesion: 0.25
Nodes (10): capture_frame(), capture_frame_sub(), encode_png(), save_screenshot(), ScreenshotMetadata, encode_png_produces_valid_png(), encode_png_1x1(), metadata_serialization_round_trip() (+2 more)

### Community 94 - "ClipRect (phantom-renderer)"
Cohesion: 0.33
Nodes (1): ClipRect

### Community 95 - "ProfileScope (phantom-app)"
Cohesion: 0.29
Nodes (5): frame_mark(), ProfileScope, profile_scope_compiles_without_feature(), profile_frame_compiles_without_feature(), frame_mark_is_callable()

### Community 96 - "CorrelationId (phantom-agents)"
Cohesion: 0.21
Nodes (13): CorrelationId, new_returns_distinct_ids(), copy_produces_equal_id(), clone_produces_equal_id(), eq_is_reflexive(), hash_is_stable_for_equal_ids(), distinct_ids_can_coexist_in_hash_set(), display_emits_hyphenated_uuid_format() (+5 more)

### Community 97 - "spawn_mock_relay() (phantom-net)"
Cohesion: 0.7
Nodes (4): handle_connection(), mock_relay_handshake_completes(), spawn_mock_relay(), two_clients_exchange_one_message()

### Community 98 - "TokenBucket (phantom-relay)"
Cohesion: 0.52
Nodes (3): TokenBucket, allows_up_to_rate(), refills_over_time()

### Community 99 - "build_codebase_context() (phantom-app)"
Cohesion: 0.5
Nodes (3): build_codebase_context(), class_label(), now_unix_ms()

### Community 100 - "App (phantom-app)"
Cohesion: 0.67
Nodes (1): App

### Community 101 - "AgentPane (phantom-app)"
Cohesion: 0.5
Nodes (1): AgentPane

### Community 102 - "RuntimeMode (phantom-agents)"
Cohesion: 0.5
Nodes (1): RuntimeMode

### Community 103 - "DagFile (phantom-dag)"
Cohesion: 0.5
Nodes (3): DagFile, from_json(), to_json()

### Community 104 - "resolve_desktop_path_unix() (phantom)"
Cohesion: 0.28
Nodes (8): probe_shell_path(), resolve_desktop_path(), resolve_desktop_path_unix(), parses_shell_path_from_output(), merges_shell_path_before_current(), timeout_kills_hung_process(), corrupt_output_without_slash_is_rejected(), empty_shell_path_is_handled()

### Community 105 - "AgentPane (phantom-app)"
Cohesion: 1.0
Nodes (1): AgentPane

### Community 106 - "is_alt_screen() (phantom-terminal)"
Cohesion: 0.67
Nodes (2): is_alt_screen(), is_vi_mode()

## Knowledge Gaps
- **13 isolated node(s):** `T`, `Extract the line number from a source_location like 'L42'.`, `Return a set of line numbers that fall inside #[cfg(test)] modules or     are #[`, `Annotate extraction nodes with ``is_test: true`` when they originate     from ```, `Parse workspace Cargo.toml for members and each crate's Cargo.toml for     inter` (+8 more)
  These have ≤1 connection - possible missing edges or undocumented components.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `crate:phantom-agents` connect `EventLog (phantom-agents)` to `AgentTask (phantom-agents, phantom-session)`, `AgentSnapshot (phantom-session)`, `InspectorAdapter (phantom-ui)`, `AgentPaneStyle (phantom-agents)`, `CorrelationId (phantom-agents)`, `SandboxError (phantom-agents)`, `RuntimeMode (phantom-agents)`, `AgentJournal (phantom-memory)`, `HeadlessActionHandler (phantom-nlp, phantom-agents)`, `AgentManager (phantom-agents)`, `QuarantineRegistry (phantom-agents)`, `SemanticParser (phantom-semantic, phantom-agents)`, `PersistentSkillRegistry (phantom-brain, phantom-agents)`, `AgentRuntime (phantom-agents, phantom-app)`, `FailureStore (phantom-agents)`, `Agent (phantom-agents)`, `GhTicketDispatcher (phantom-agents)`?**
  _High betweenness centrality (0.000) - this node is a cross-community bridge._
- **Why does `crate:phantom-app` connect `TerminalAdapter (phantom-app, phantom-scene)` to `AgentTask (phantom-agents, phantom-session)`, `InspectorAdapter (phantom-ui)`, `MemoryStore (phantom-brain)`, `PluginRegistry (phantom-renderer)`, `EventLog (phantom-agents)`, `Bundle (phantom-bundle-store, phantom-bundles)`, `PhantomTerminal (phantom-terminal, phantom-app)`, `AgentJournal (phantom-memory)`, `HeadlessActionHandler (phantom-nlp, phantom-agents)`, `AgentManager (phantom-agents)`, `OpenAiBackend (phantom-stt)`, `MockRuntime (phantom-plugins)`, `SemanticParser (phantom-semantic, phantom-agents)`, `PersistentSkillRegistry (phantom-brain, phantom-agents)`, `AgentRuntime (phantom-agents, phantom-app)`, `KeybindRegistry (phantom-ui, phantom-app)`, `Screenshot (phantom-vision)`, `Supervisor (phantom-supervisor, phantom-app)`, `TextRenderer (phantom-renderer)`, `AppCoordinator (phantom-app)`, `BootSequence (phantom-app)`, `CodeDag (phantom-dag, phantom-app)`, `PhantomConfig (phantom, phantom-app)`, `AgentSnapshot (phantom-session)`, `SysmonHandle (phantom-app)`, `HistoryStore (phantom-history)`, `InMemoryStore (phantom-embeddings)`, `SelfTestRunner (phantom-app)`, `ResourceManager (phantom-app)`, `SettingsPanel (phantom-app)`, `McpClient (phantom-mcp)`, `Console (phantom-app)`, `AgentAdapter (phantom-app)`, `JobPool (phantom-app)`, `App (phantom-app)`, `AgentPane (phantom-app)`, `Framework (phantom-context)`, `PhantomLogger (phantom-app)`, `MonitorAdapter (phantom-app)`, `PhantomSettings (phantom-app)`, `App (phantom-app)`, `NotificationCenter (phantom-app)`, `ShutdownGuard (phantom-app)`, `SupervisorClient (phantom-app)`, `App (phantom-app)`, `ProfileScope (phantom-app)`, `build_codebase_context() (phantom-app)`, `App (phantom-app)`, `AgentPane (phantom-app)`, `AgentPane (phantom-app)`?**
  _High betweenness centrality (0.000) - this node is a cross-community bridge._
- **Why does `crate:phantom-brain` connect `MemoryStore (phantom-brain)` to `AgentTask (phantom-agents, phantom-session)`, `TerminalAdapter (phantom-app, phantom-scene)`, `PluginRegistry (phantom-renderer)`, `EventLog (phantom-agents)`, `AgentJournal (phantom-memory)`, `SemanticParser (phantom-semantic, phantom-agents)`, `PersistentSkillRegistry (phantom-brain, phantom-agents)`, `AgentRuntime (phantom-agents, phantom-app)`, `InterventionEngine (phantom-brain)`, `ClaudeBackend (phantom-brain)`, `TaskLedger (phantom-brain)`, `GoalPursuit (phantom-brain)`, `Attention (phantom-adapter, phantom-brain)`, `HistoryStore (phantom-history)`, `ProviderCatalog (phantom-brain)`, `DecompositionStep (phantom-brain)`, `Framework (phantom-context)`, `ReconcilerState (phantom-brain)`, `GenerateOptions (phantom-brain)`?**
  _High betweenness centrality (0.000) - this node is a cross-community bridge._
- **What connects `T`, `Extract the line number from a source_location like 'L42'.`, `Return a set of line numbers that fall inside #[cfg(test)] modules or     are #[` to the rest of the system?**
  _13 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `AgentTask (phantom-agents, phantom-session)` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `TerminalAdapter (phantom-app, phantom-scene)` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `InspectorAdapter (phantom-ui)` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._