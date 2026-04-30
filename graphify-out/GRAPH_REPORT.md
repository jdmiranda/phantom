# Graph Report - .  (2026-04-30)

## Corpus Check
- 282 files · ~498,044 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 7323 nodes · 15153 edges · 162 communities detected
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## God Nodes (most connected - your core abstractions)
1. `AppCoordinator` - 47 edges
2. `parse_agent_command()` - 43 edges
3. `Agent` - 38 edges
4. `BootSequence` - 35 edges
5. `register_mock()` - 30 edges
6. `agent_with_handle()` - 30 edges
7. `rust_ctx()` - 30 edges
8. `TerminalAdapter` - 29 edges
9. `tmp()` - 29 edges
10. `PhantomTerminal` - 28 edges

## Surprising Connections (you probably didn't know these)
- `from_env_succeeds_with_key()` --calls--> `set_env()`  [EXTRACTED]
  crates/phantom-embeddings/src/openai.rs → crates/phantom-voice/src/openai.rs
- `main()` --calls--> `print_banner()`  [EXTRACTED]
  crates/phantom/src/main.rs → crates/phantom-supervisor/src/main.rs
- `main()` --calls--> `parse_config_flag()`  [EXTRACTED]
  crates/phantom/src/main.rs → crates/phantom-relay/src/main.rs
- `from_env_returns_not_configured_when_missing()` --calls--> `env_lock()`  [EXTRACTED]
  crates/phantom-embeddings/src/openai.rs → crates/phantom-voice/src/openai.rs
- `from_env_succeeds_with_key()` --calls--> `env_lock()`  [EXTRACTED]
  crates/phantom-embeddings/src/openai.rs → crates/phantom-voice/src/openai.rs

## Communities

### Community 0 - "Community 0"
Cohesion: 0.01
Nodes (243): DagEdge, EdgeKind, adapter_event_adapter_id_all_variants(), adapter_id_is_copy_at_registration_boundary(), add_edge_increments_count(), add_methods_preserve_insertion_order(), add_node_stores_and_retrieves(), app_core_adapter_id_default_is_sentinel() (+235 more)

### Community 1 - "Community 1"
Cohesion: 0.02
Nodes (99): Agent, agent_adapter_accepts_input(), agent_adapter_app_type_is_agent(), agent_adapter_is_visual(), agent_adapter_render_stays_within_rect(), agent_adapter_scroll_offset_starts_at_zero(), agent_adapter_with_spawn_tag_stores_tag(), agent_get_state_exposes_history_size() (+91 more)

### Community 2 - "Community 2"
Cohesion: 0.02
Nodes (69): args_hash_is_deterministic(), AuditOutcome, AuditWriter, emit_before_init_does_not_panic(), emit_tool_call(), hash_args(), init(), init_then_emit_writes_record_and_drop_flushes() (+61 more)

### Community 3 - "Community 3"
Cohesion: 0.04
Nodes (79): add_and_query_facts(), approve_checkpoint_makes_step_eligible(), approve_checkpoint_wrong_step_returns_err(), assess_complete_when_all_done(), assess_not_complete_with_pending(), checkpoint_step_is_not_eligible_until_approved(), detect_loop_on_repeated_outputs(), dispatch_next_step_is_idempotent() (+71 more)

### Community 4 - "Community 4"
Cohesion: 0.02
Nodes (56): AppAdapter, AppCore, BusParticipant, Commandable, CursorData, CursorShape, GridData, InputHandler (+48 more)

### Community 5 - "Community 5"
Cohesion: 0.03
Nodes (70): ActionClass, bds_idle_wins_when_no_errors(), bds_momentum_prevents_thrashing(), bds_picks_highest_scorer(), Behavior, behavior_additive_composition(), behavior_clamped_to_class_range(), BehaviorDecisionSystem (+62 more)

### Community 6 - "Community 6"
Cohesion: 0.03
Nodes (74): api_handle_detects_disconnected_sender(), api_handle_marks_done_on_done_event(), api_handle_marks_done_on_error_event(), api_handle_returns_none_after_done(), ApiEvent, ApiHandle, build_messages(), build_messages_consecutive_tool_calls_grouped() (+66 more)

### Community 7 - "Community 7"
Cohesion: 0.05
Nodes (67): agent_context_empty(), agent_context_formatting(), all_returns_every_entry(), append_increments_count(), by_category_filters_correctly(), by_session_filters_entries(), by_time_range_filters_correctly(), corrupt_lines_skipped() (+59 more)

### Community 8 - "Community 8"
Cohesion: 0.05
Nodes (40): adapter_render_height_excludes_title_strip(), adapter_render_y_starts_below_title_strip(), AppCoordinator, arbiter_allocation_constrains_taffy_pane(), coordinator_passes_inner_rect_to_adapter(), LayeredRenderOutputs, LayoutPlan, MockAdapter (+32 more)

### Community 9 - "Community 9"
Cohesion: 0.04
Nodes (63): agent_ref(), agent_row_excerpt_truncates_to_80_chars(), agent_row_serializes_with_flattened_agent_ref_fields(), agent_row_spawned_minutes_ago_saturates_when_clock_goes_backward(), agent_row_spawned_minutes_ago_uses_injected_now(), AgentRow, build_view_with_agents(), builder_caps_denials_dropping_oldest() (+55 more)

### Community 10 - "Community 10"
Cohesion: 0.04
Nodes (58): agent_is_quarantined(), quarantine_registry_blocks(), source_agent_id_from_envelope(), taint_from_source_chain(), defender_challenge_loop_smoke(), make_agent(), new_spawn_queue(), offender_cannot_call_challenge_agent() (+50 more)

### Community 11 - "Community 11"
Cohesion: 0.05
Nodes (69): capture_frame_accessors_return_correct_values(), CaptureFrame, CaptureSession, cleanup_deletes_old_sessions(), cleanup_returns_zero_when_nothing_to_delete(), consecutive_duplicate_phash_frames_are_deduplicated(), crash_and_restart_recovers_last_saved_layout(), different_projects_get_different_hashes() (+61 more)

### Community 12 - "Community 12"
Cohesion: 0.03
Nodes (50): GlyphAtlas, GlyphEntry, default_grid_cell_has_transparent_bg(), grid_cell_to_terminal_cell(), GridCell, GridRenderData, GridRenderer, is_default_bg() (+42 more)

### Community 13 - "Community 13"
Cohesion: 0.04
Nodes (37): AgentPane, AgentPaneStatus, bar_rect(), ConnectionIndicator, dispatch_tool(), result(), status_bar_defaults(), status_bar_renders_background_quad() (+29 more)

### Community 14 - "Community 14"
Cohesion: 0.05
Nodes (59): defender_spawn_rule(), defender_spawn_rule_does_not_fire_on_agent_blocked(), defender_spawn_rule_does_not_fire_on_pane_opened(), defender_spawn_rule_fires_on_capability_denied(), defender_spawn_rule_uses_spawn_if_not_running(), ev(), ev(), fixer_deadline_payload() (+51 more)

### Community 15 - "Community 15"
Cohesion: 0.03
Nodes (64): agent_pane_starts_working(), agent_with_handle(), audit_denied_outcome_emitted_alongside_event(), blocked_event_payload_has_agent_id_and_reason(), blocked_payload_reflects_actual_role_and_capability(), build_ctx(), build_dispatch_context_passes_quarantine_registry_to_dispatch(), capability_denied_source_chain_propagates_taint() (+56 more)

### Community 16 - "Community 16"
Cohesion: 0.04
Nodes (40): AppActionHandler, Console, console_with_3_commands(), ConsoleLine, duplicate_adjacent_commands_deduplicated(), history_down_moves_forward(), history_down_to_end_restores_draft(), history_down_when_not_navigating_is_no_op() (+32 more)

### Community 17 - "Community 17"
Cohesion: 0.04
Nodes (81): AgentCommand, duplicate_capability_flag_last_value_wins_with_warning(), duplicate_model_flag_last_value_wins_with_warning(), duplicate_role_flag_last_value_wins_with_warning(), duplicate_ttl_flag_last_value_wins_with_warning(), execute_agent_command(), execute_help_shows_usage(), execute_kill_all_kills_all_active() (+73 more)

### Community 18 - "Community 18"
Cohesion: 0.05
Nodes (50): hook_context_command_builder(), hook_context_error_builder(), hook_context_output_builder(), hook_type_key(), HookContext, HookEvent, HookResponse, mock_runtime_command() (+42 more)

### Community 19 - "Community 19"
Cohesion: 0.03
Nodes (40): App, AppState, build_ticket_dispatcher(), FloatInteraction, NlpTranslateResult, open_bundle_store(), ResizeEdge, SuggestionOverlay (+32 more)

### Community 20 - "Community 20"
Cohesion: 0.04
Nodes (59): handshake(), make_send(), rate_limiter_trips_on_burst_without_disconnect(), recv_json(), spawn_relay(), two_peers_rendezvous_round_trip(), at_in_middle_is_not_a_mention(), at_label_no_body_returns_empty_body() (+51 more)

### Community 21 - "Community 21"
Cohesion: 0.05
Nodes (53): ContextMenu, hide_clears_state(), hit_test_below_items(), hit_test_inside_first_item(), hit_test_inside_second_item(), hit_test_outside_x(), hit_test_when_hidden(), MenuAction (+45 more)

### Community 22 - "Community 22"
Cohesion: 0.05
Nodes (37): active_is_demoted_to_pending_on_restore(), active_step(), active_step_demoted_on_restore(), atomic_write_creates_no_temp_file_after_success(), done_is_preserved_on_restore(), done_step(), done_step_count_correct(), done_steps_remain_done_after_restore() (+29 more)

### Community 23 - "Community 23"
Cohesion: 0.06
Nodes (38): AppRegistry, dirs_or_default(), empty_registry_invoke_returns_unknown_tool(), empty_registry_resolve_returns_none(), handle_call_response_error_propagates_mcp_error(), handle_call_response_success(), invoke_known_tool_returns_ok(), invoke_unknown_tool_returns_mcp_error() (+30 more)

### Community 24 - "Community 24"
Cohesion: 0.03
Nodes (16): convert_cursor(), convert_cursor_shape(), default_is_not_alt_screen(), default_mouse_mode_is_none(), key_name_to_bytes(), last_read_buf_empty_before_any_read(), last_read_buf_returns_only_valid_bytes_after_would_block(), MouseMode (+8 more)

### Community 25 - "Community 25"
Cohesion: 0.05
Nodes (68): args_hash_deterministic(), available_tools(), available_tools_returns_all_eight(), DispatchError, edit_file_fails_on_multiple_matches(), edit_file_fails_on_no_match(), edit_file_rejects_traversal(), edit_file_replaces_unique_match() (+60 more)

### Community 26 - "Community 26"
Cohesion: 0.05
Nodes (49): alice(), bob(), canonical_bytes(), ClientMessage, deserialize(), Engine, Envelope, envelope_nonce_increments() (+41 more)

### Community 27 - "Community 27"
Cohesion: 0.06
Nodes (50): acceptance_rate_tracks_feedback(), build_error_does_not_fire_on_success(), build_error_fires_on_cargo_build_failure(), build_error_suggest_contains_action_and_rationale(), confidence_is_clamped_to_unit_range(), context_change_does_not_fire_on_file_changed(), context_change_fires_on_git_state_changed(), cooldown_expires_after_configured_ms() (+42 more)

### Community 28 - "Community 28"
Cohesion: 0.05
Nodes (53): App, intent_to_translate_result(), config_path(), default_skip_boot_is_false(), nlp_llm_enabled_defaults_to_true(), parse_empty_config_yields_defaults(), parse_ignores_comments_and_blank_lines(), parse_nlp_llm_false_disables_it() (+45 more)

### Community 29 - "Community 29"
Cohesion: 0.06
Nodes (38): append(), append_hex(), append_usize(), Config, dirs_or_home(), find_swift_runtime_path(), inject_dyld_fallback(), install_atexit() (+30 more)

### Community 30 - "Community 30"
Cohesion: 0.08
Nodes (45): DagNode, NodeKind, RenderLayer, SceneNode, Transform, WorldTransform, add_multiple_children(), add_nested_children() (+37 more)

### Community 31 - "Community 31"
Cohesion: 0.06
Nodes (44): assembled_bundle_json_round_trips(), AssemblerError, BundleAssembler, finish_audio_only_bundle_is_valid(), finish_frame_only_bundle_is_valid(), finish_importance_is_clamped(), finish_on_empty_assembler_errors(), finish_preserves_insertion_order_across_all_modalities() (+36 more)

### Community 32 - "Community 32"
Cohesion: 0.04
Nodes (37): App, bracketed_paste(), bracketed_paste_empty(), bracketed_paste_multiline(), ctrl_alt_char(), encode_arrow(), encode_char(), encode_function_key() (+29 more)

### Community 33 - "Community 33"
Cohesion: 0.07
Nodes (33): BootPhase, BootSequence, BootTextLine, build_syscheck_text(), crt_intensity_ramps_during_warmup(), default_is_new(), done_phase_does_not_advance(), initial_state() (+25 more)

### Community 34 - "Community 34"
Cohesion: 0.06
Nodes (35): all_fields_accessible_via_getters(), all_returns_every_notification_in_insertion_order(), Banner, by_kind_filters_correctly(), count_tracks_push_and_remove(), different_projects_are_isolated(), does_not_trigger_below_threshold(), get_existing_id_returns_some() (+27 more)

### Community 35 - "Community 35"
Cohesion: 0.05
Nodes (36): clear_env(), dimension_for_text_returns_default_3072(), embed_hello_world_returns_two_3072_dim_vectors(), embed_image_returns_unsupported_modality(), EmbeddingData, EmbeddingsRequest, EmbeddingsResponse, env_lock() (+28 more)

### Community 36 - "Community 36"
Cohesion: 0.05
Nodes (41): build_openai_embeddings(), capture_pipeline_command_boundary_seals_bundle(), capture_pipeline_dedup_hits_skip_writes(), capture_pipeline_skips_when_bundle_store_missing(), capture_state_advances_frame_counter_per_pane(), CaptureState, crate::app::App, EmbeddingBackendKind (+33 more)

### Community 37 - "Community 37"
Cohesion: 0.05
Nodes (36): ErrorHighlighter, extract_refs_deduplicates(), extract_refs_from_cargo_error(), highlight_produces_regions_for_errors(), highlight_warning_uses_yellow(), HighlightRegion, HighlightStyle, scan_file_line_no_col() (+28 more)

### Community 38 - "Community 38"
Cohesion: 0.06
Nodes (42): activate_deactivate_commands(), app_type_returns_monitor(), does_not_accept_input(), fake_handle(), fake_stats(), get_state_reflects_active_flag(), is_always_alive(), is_not_visual() (+34 more)

### Community 39 - "Community 39"
Cohesion: 0.06
Nodes (59): agent_context_contains_key_fields(), collect_git_info(), DispatchContext, extract_name_falls_back_to_dir(), extract_name_from_package_json(), extract_project_name(), git_info_in_real_repo(), GitInfo (+51 more)

### Community 40 - "Community 40"
Cohesion: 0.05
Nodes (30): AlwaysAvailableBackend, chat_options_default_values(), ChatBackend, ChatOptions, ChatResponse, claude_backend_availability_reflects_env_var(), claude_backend_is_cloud(), claude_backend_provider_name() (+22 more)

### Community 41 - "Community 41"
Cohesion: 0.06
Nodes (44): adapter_event_adapter_id_consistent_across_all_variants(), adapter_event_clone_and_equality(), adapter_event_closed_variant(), adapter_event_content_changed_variant(), adapter_event_debug_format(), adapter_event_focused_variant(), adapter_event_spawned_variant(), adapter_id_copy_semantics() (+36 more)

### Community 42 - "Community 42"
Cohesion: 0.06
Nodes (32): build_and_start_stream(), enumerate_succeeds_or_permission_denied(), MacOsSckCapture, map_sc_error(), map_sc_error_with_context(), non_app_stream_open_returns_unsupported(), permission_status_returns_a_known_variant(), sample_buffer_to_audio_frame() (+24 more)

### Community 43 - "Community 43"
Cohesion: 0.08
Nodes (39): AgentSnapshot, AgentStateFile, AgentStatePersister, atomic_write_creates_no_temp_file_after_success(), awaiting_approval_normalised_to_queued(), completed_tool_pair_is_preserved(), conversation_depth_excludes_system_messages(), discard_nonexistent_is_ok() (+31 more)

### Community 44 - "Community 44"
Cohesion: 0.09
Nodes (30): AgentJournal, append_pre_built_entry_preserves_all_fields(), each_file_line_is_valid_journal_entry_json(), entry_from_envelope(), filter_by_agent_and_phase_intersection(), filter_by_agent_isolates_per_agent(), filter_by_level_returns_matching(), filter_by_phase_returns_only_matching() (+22 more)

### Community 45 - "Community 45"
Cohesion: 0.06
Nodes (30): all_blocked_returns_none(), blocked_ticket_is_not_returned(), capability_matches_label(), ClaimedSet, concurrent_requests_get_distinct_tickets(), dispatcher_tool_api_name_round_trip(), dispatcher_with(), DispatcherTool (+22 more)

### Community 46 - "Community 46"
Cohesion: 0.07
Nodes (42): ambiguous_input_clarifies(), build_maps_to_node_command(), build_system_prompt(), build_the_project_runs_cargo_build(), clarify_accessors(), claude_backend_name_is_claude(), ClaudeLlmBackend, empty_backend_reply_maps_to_clarify() (+34 more)

### Community 47 - "Community 47"
Cohesion: 0.06
Nodes (46): absolute_path_passes_through(), bare_deploy_is_ambiguous(), build_resolves_to_node_command(), build_resolves_to_rust_command(), ci_status_shows_info(), deploy_production_normalizes_prod(), deploy_staging_resolves(), empty_input_passes_through() (+38 more)

### Community 48 - "Community 48"
Cohesion: 0.08
Nodes (36): append_challenge_to_log(), challenge_agent(), challenge_agent_appends_envelope_to_log_when_configured(), challenge_agent_delivers_message_to_target_inbox(), challenge_agent_denied_for_role_lacking_coordinate(), challenge_agent_full_inbox_returns_err_not_panic(), challenge_agent_round_trip_defender_sends_target_replies(), challenge_agent_unknown_target_returns_err() (+28 more)

### Community 49 - "Community 49"
Cohesion: 0.08
Nodes (23): AgentRef, default_skills_path(), dirs_home(), file_persistence_survives_reload(), lookup_unknown_returns_none(), PersistentSkillRegistry, record_outcome_unknown_id_is_noop(), record_outcome_updates_counts() (+15 more)

### Community 50 - "Community 50"
Cohesion: 0.08
Nodes (44): append_speak_to_log(), broadcast_to_role(), broadcast_to_role_returns_recipient_count(), broadcast_to_role_unknown_role_returns_err(), broadcast_to_role_with_no_matches_returns_zero(), BroadcastArgs, build_ctx(), chat_tool_api_name_round_trip() (+36 more)

### Community 51 - "Community 51"
Cohesion: 0.1
Nodes (39): accent_bar_is_at_strip_bottom(), accent_bar_uses_accent_focus_token(), active_tab_bg_uses_surface_raised(), active_tab_text_uses_text_primary(), badge_count_updates_when_tab_receives_event(), badge_renders_as_additional_segment_in_status_warn(), badge_zero_is_distinct_from_no_badge(), click_does_not_change_other_tab_badges() (+31 more)

### Community 52 - "Community 52"
Cohesion: 0.07
Nodes (23): build_call_tool_request(), build_call_tool_request_correct_shape(), build_initialize_produces_valid_request(), build_initialize_request(), build_list_tools_request(), build_list_tools_request_correct_method(), builder_ids_are_unique(), call_tool_add_round_trip() (+15 more)

### Community 53 - "Community 53"
Cohesion: 0.08
Nodes (46): chunked_transfer_assembles_correctly(), delete_clears_pending_chunks(), ImageHandler, make_cmd(), png_format_decoding(), rgb_format_decoding(), single_rgba_transmit(), base64_decode() (+38 more)

### Community 54 - "Community 54"
Cohesion: 0.1
Nodes (35): background_quad_uses_surface_recessed(), backspace_at_start_is_no_op(), backspace_removes_char_before_cursor(), bare_bar(), buffer_text_uses_text_primary(), cursor_in_middle_inserts_at_cursor(), cursor_quad_uses_text_primary_when_visible(), delete_at_end_is_no_op() (+27 more)

### Community 55 - "Community 55"
Cohesion: 0.06
Nodes (27): all_templates_are_distinct(), Analysis, analysis_embedding_storable_as_vec_f32(), ChatRequest, ChatResponse, Choice, ContentPart, ImageUrl (+19 more)

### Community 56 - "Community 56"
Cohesion: 0.07
Nodes (23): assert_duration_approx(), Cadence, cadence_fires_at_target_hz(), cadence_skips_when_too_soon(), cadence_unlimited_always_ticks(), cadence_zero_hz_never_ticks(), Clock, clock_advances_monotonic() (+15 more)

### Community 57 - "Community 57"
Cohesion: 0.13
Nodes (36): agent_error_none_spawn_tag_is_not_coerced_to_zero(), agent_error_with_none_spawn_tag_is_noop(), auto_approve_disposition_skips_awaiting_approval(), connection_state_for_reason(), dag_aware_dispatch_skips_blocked_step(), dag_dispatch_is_idempotent_for_active_steps(), dag_dispatch_unblocks_after_dep_done(), dispatch_and_capture_tag() (+28 more)

### Community 58 - "Community 58"
Cohesion: 0.11
Nodes (32): click_above_track_clamps_to_max(), click_below_track_clamps_to_zero(), click_bottom_gives_zero_offset(), click_middle_gives_half_offset(), click_top_gives_max_offset(), click_zero_height_always_zero(), click_zero_history_always_zero(), no_quads_when_not_scrollable() (+24 more)

### Community 59 - "Community 59"
Cohesion: 0.09
Nodes (26): catalog_custom_profile_overrides_builtin(), catalog_default_profile_has_sonnet(), catalog_get_builtin_fast_profile(), catalog_get_missing_profile_returns_none(), default_equals_with_builtins(), empty_catalog_has_no_profiles(), empty_catalog_resolve_returns_none_for_unknown(), insert_adds_new_profile() (+18 more)

### Community 60 - "Community 60"
Cohesion: 0.08
Nodes (31): all_official_plugins_have_wasm_entry_point(), api_inspector_hooks_on_output(), docker_dashboard_has_status_bar(), get_official(), get_official_finds_existing_plugin(), git_enhanced_has_correct_hooks_and_commands(), github_notifications_has_all_three_permissions(), official_plugin_names_are_unique() (+23 more)

### Community 61 - "Community 61"
Cohesion: 0.09
Nodes (30): build_decomposition_prompt(), ChatBackend, ClaudeChatBackend, decompose(), decompose_all_steps_are_pending_initially(), decompose_error_on_backend_failure(), decompose_error_when_no_steps_parsed(), decompose_produces_task_ledger_matching_plan() (+22 more)

### Community 62 - "Community 62"
Cohesion: 0.09
Nodes (27): bootstrap_proposes_first_task(), bootstrap_skips_completed_tasks(), build_prompt_includes_project_name(), CurriculumGenerator, CurriculumHook, next_difficulty_clamps_at_ten(), next_difficulty_from_one(), next_difficulty_increments_by_one() (+19 more)

### Community 63 - "Community 63"
Cohesion: 0.09
Nodes (21): BrainTask, build_reflection_prompt(), build_task_creation_prompt(), goal_pursuit_lifecycle(), goal_pursuit_summary(), GoalPursuit, inject_reflections(), parse_task_list() (+13 more)

### Community 64 - "Community 64"
Cohesion: 0.1
Nodes (36): allocate_subagent_id(), append(), build_critique_body(), build_critique_body_contains_all_required_tokens(), completion_kinds_for(), composer_ref(), composer_tool_api_name_round_trip(), ComposerTool (+28 more)

### Community 65 - "Community 65"
Cohesion: 0.17
Nodes (30): agent_role_color_matches_text_accent(), all_role_colors_are_distinct(), avatar_and_label_use_role_color(), avatar_segment_uses_role_initial(), body_exactly_cols_wide_fits_one_line(), body_one_over_cols_wraps_to_two_lines(), body_segments_use_text_primary_color(), body_text_is_offset_past_avatar_column() (+22 more)

### Community 66 - "Community 66"
Cohesion: 0.13
Nodes (24): achievability_check_true_when_steps_exist(), all_steps_have_valid_fields(), cost_estimation_is_sum_of_step_costs(), decompose_and_replan_installs_plan_on_trigger(), decompose_and_replan_returns_none_when_steady(), DecompositionResult, DecompositionStep, default_world() (+16 more)

### Community 67 - "Community 67"
Cohesion: 0.13
Nodes (26): all_empty_slots_produce_no_text(), all_three_slots_render_when_content_fits(), background_color_is_surface_recessed_token(), center_slot_x_is_within_center_third(), default_has_empty_slots(), empty_slots_are_omitted(), left_slot_x_position(), medium_width_short_labels_not_truncated() (+18 more)

### Community 68 - "Community 68"
Cohesion: 0.14
Nodes (27): CorpusEntry, cosine(), empty_corpus_returns_empty_results(), InMemoryRecallEngine, long_corpus_embedding_is_truncated(), meta(), min_score_boundary_inclusive(), min_score_filters_low_similarity_entries() (+19 more)

### Community 69 - "Community 69"
Cohesion: 0.16
Nodes (26): body_rect_is_strictly_inside_outer_rect_titled(), body_rect_is_strictly_inside_outer_rect_untitled(), border_quads_use_chrome_frame_token(), border_thickness_equals_frame_token(), bottom_border_positioned_at_outer_bottom(), default_is_untitled(), degenerate_rect_body_clamped_to_zero(), divider_uses_chrome_divider_token() (+18 more)

### Community 70 - "Community 70"
Cohesion: 0.15
Nodes (28): always_emits_four_quads(), clear_focus_leaves_no_pane_focused(), clear_focused_sets_none(), cycling_focus_between_panes(), default_matches_new(), focus_pane_b_is_reported(), focus_ring_is_object_safe(), focus_transfers_from_a_to_c() (+20 more)

### Community 71 - "Community 71"
Cohesion: 0.1
Nodes (17): Action, bare_key_no_modifiers(), bind_override(), default_bindings_present(), display_key_combo(), iter_yields_all_bindings(), Key, key_combo_equality() (+9 more)

### Community 72 - "Community 72"
Cohesion: 0.1
Nodes (25): b1_cooldown_blocks_rapid_actions(), b1_dedup_suppresses_identical_suggestion(), b1_suggestions_since_input_dampens(), b1_watcher_score_zero_without_active_process(), b226_command_complete_with_real_output_yields_nonzero_fix_score(), error_output(), test_memory(), TestAdapter (+17 more)

### Community 73 - "Community 73"
Cohesion: 0.13
Nodes (20): additional_taints_on_quarantined_agent_return_false(), AgentRuntime, AgentSlot, AutoQuarantinePolicy, clean_taint_resets_consecutive_counter(), custom_threshold_one_quarantines_immediately(), denial_counter_resets_after_release(), n_minus_one_taints_do_not_quarantine() (+12 more)

### Community 74 - "Community 74"
Cohesion: 0.16
Nodes (15): chrome_regions_fill_window(), LayoutEngine, PaneId, Rect, remove_pane_decreases_count(), resize_updates_layout(), set_pane_size_constraints_clear_restores_auto(), set_pane_size_constraints_updates_taffy_style() (+7 more)

### Community 75 - "Community 75"
Cohesion: 0.13
Nodes (12): clear_empties_queue(), DebugDrawManager, DebugPrimitive, disabled_flush_clears_queue(), disabled_manager_ignores_adds(), draw_options_builders(), DrawOptions, enabled_manager_queues_primitives() (+4 more)

### Community 76 - "Community 76"
Cohesion: 0.16
Nodes (23): call_missing_export_returns_err_not_panic(), host(), load_and_call_add_returns_correct_result(), load_invalid_bytes_returns_err(), load_with_different_arg_values(), module_with_no_imports_loads_successfully(), plugin_runtime_call_hook_returns_none_when_no_export(), plugin_runtime_call_hook_returns_none_when_ph_on_hook_returns_zero() (+15 more)

### Community 77 - "Community 77"
Cohesion: 0.08
Nodes (22): embedding(), reopen_after_clean_write_does_not_error(), sweep_cleans_orphaned_vectors_and_leaked_rows_table(), sweep_drops_unknown_vector_entries(), sweep_leaked_rows(), sweep_with_orphaned_vector_drops_it(), all_bundle_ids(), check_schema_version() (+14 more)

### Community 78 - "Community 78"
Cohesion: 0.13
Nodes (22): default_is_horizontal(), Divider, divider_is_object_safe(), h_rect(), horizontal_color_matches_chrome_divider_token(), horizontal_constructor_sets_orientation_and_hair_thickness(), horizontal_emits_no_text(), horizontal_preferred_size_is_height_of_divider() (+14 more)

### Community 79 - "Community 79"
Cohesion: 0.14
Nodes (19): attach_same_key_returns_shared_block(), Block, block_key_distinguishes_namespace_and_label(), BlockError, BlockHandle, BlockKey, drop_block_removes_and_get_returns_none(), get_on_missing_key_returns_none() (+11 more)

### Community 80 - "Community 80"
Cohesion: 0.11
Nodes (14): AgentOutputCapture, AgentRecord, append_increments_count(), by_agent_filters_correctly(), CapturedAgent, corrupt_lines_skipped(), default_data_dir(), empty_capture_is_safe() (+6 more)

### Community 81 - "Community 81"
Cohesion: 0.13
Nodes (17): async_load_flow(), failed_load(), gc_keeps_referenced(), load_returns_handle(), LoadStatus, make_test_resource(), memory_usage_sums(), release_and_gc() (+9 more)

### Community 82 - "Community 82"
Cohesion: 0.17
Nodes (17): builder_adds_failed_attempts_batch(), builder_adds_failed_attempts_one_by_one(), builder_adds_memory_refs(), builder_adds_memory_refs_batch(), builder_correlation_id_absent_by_default(), builder_empty_failed_attempts(), builder_sets_correlation_id(), builder_sets_required_fields() (+9 more)

### Community 83 - "Community 83"
Cohesion: 0.15
Nodes (24): composed_chrome_reserves_title_and_footer(), layout_box_is_copy_clone_debug(), LayoutBox, pad_clamps_to_zero_when_excessive(), pad_insets_all_four_sides(), pad_zero_is_noop(), parent(), reserve_bottom_carves_strip_and_remainder() (+16 more)

### Community 84 - "Community 84"
Cohesion: 0.14
Nodes (31): ansi_color_table(), ansi_palette_override_applied(), ansi_palette_override_only_affects_0_to_15(), background_uses_theme_color_not_transparent(), bright_foreground_uses_theme_color(), cell_flags_all_underline_variants(), cell_flags_from_alac_round_trips(), cell_flags_wide_char() (+23 more)

### Community 85 - "Community 85"
Cohesion: 0.12
Nodes (29): AgentSuggestion, build_context(), build_error_summary(), build_error_summary_includes_code_and_location(), build_error_summary_without_code_or_column(), build_task(), build_task_context_includes_command_and_dir(), build_task_falls_back_to_first_error_without_file() (+21 more)

### Community 86 - "Community 86"
Cohesion: 0.15
Nodes (28): actor_does_not_have_broadcast_tool(), actor_gets_all_tools_including_act(), all_mcp_tools(), all_mcp_tools_covers_every_advertised_method(), capturer_does_not_have_broadcast_tool(), capturer_gets_sense_and_reflect_tools(), composer_also_has_broadcast_tool(), conversational_gets_all_minus_act() (+20 more)

### Community 87 - "Community 87"
Cohesion: 0.13
Nodes (15): agent_complete_triggers_notification(), custom_high_threshold_suppresses_action(), default_loop(), error_world(), high_score_action_is_dispatched(), low_score_action_is_skipped(), metrics_record_last_winner(), metrics_tick_counter_increments() (+7 more)

### Community 88 - "Community 88"
Cohesion: 0.14
Nodes (22): backward_time_does_not_toggle(), carry_over_is_preserved_after_toggle(), color_passes_through_base_when_visible(), color_zeroes_alpha_when_hidden(), CursorBlink, custom_period_toggles_at_custom_boundary(), default_period_is_530ms(), default_starts_visible() (+14 more)

### Community 89 - "Community 89"
Cohesion: 0.16
Nodes (16): accessors_expose_all_fields(), agents_do_not_bleed_into_each_other(), by_agent_returns_only_matching_records(), clear_agent_removes_failures_for_that_agent(), default_produces_empty_store(), FailureRecord, FailureStore, free_form_task() (+8 more)

### Community 90 - "Community 90"
Cohesion: 0.22
Nodes (18): active_count_tracks_working_agents(), AgentManager, agents_returns_all(), by_status_filters(), cleanup_keeps_active_agents(), cleanup_promotes_queued_to_working(), cleanup_removes_old_completed_agents(), free_task() (+10 more)

### Community 91 - "Community 91"
Cohesion: 0.13
Nodes (27): accept_loop(), AppCommand, dispatch(), dispatch_get_context(), dispatch_get_memory(), dispatch_phantom_command(), dispatch_read_output(), dispatch_run_command() (+19 more)

### Community 92 - "Community 92"
Cohesion: 0.09
Nodes (14): decoded_image_from_png_grayscale(), decoded_image_from_png_invalid_bytes(), decoded_image_from_png_rgb(), decoded_image_from_png_valid(), decoded_image_from_rgb(), decoded_image_from_rgb_too_short(), decoded_image_from_rgba(), decoded_image_from_rgba_too_short() (+6 more)

### Community 93 - "Community 93"
Cohesion: 0.15
Nodes (23): click_drag_left_to_right_forward(), click_drag_right_to_left_normalizes(), click_drag_same_row_backward(), collapsed_when_same_point(), ctx(), flowing_four_rows_four_rects(), flowing_single_row_one_rect(), flowing_three_rows_three_rects() (+15 more)

### Community 94 - "Community 94"
Cohesion: 0.15
Nodes (14): by_task_filters_by_task_id(), HandoffEntry, HandoffError, HandoffLog, jsonl_round_trip_survives_reopen(), mk_log(), multi_agent_chain_all_under_same_task_id(), new_entry() (+6 more)

### Community 95 - "Community 95"
Cohesion: 0.16
Nodes (12): agent_event_carries_task_and_success(), app_topology_three_topics(), BusMessage, DataType, drain_clears_queue_for_subscriber(), emit_caps_queue_at_max_size(), error_event_isolated_from_output_subscriber(), EventBus (+4 more)

### Community 96 - "Community 96"
Cohesion: 0.17
Nodes (18): decode_png_with_meta(), decode_rejects_non_png(), dhash_collision_similar_frames_within_hamming_4(), dhash_very_different_frames_exceed_hamming_4(), encode_rgba_to_png(), gradient_rgba(), new_rejects_size_mismatch(), new_rejects_zero_dimensions() (+10 more)

### Community 97 - "Community 97"
Cohesion: 0.13
Nodes (9): builder_defaults(), builder_setters(), corrupt_line_returns_err(), HistoryEntry, HistoryEntryBuilder, jsonl_round_trip(), make_entry(), timestamp_iso8601_round_trip() (+1 more)

### Community 98 - "Community 98"
Cohesion: 0.16
Nodes (10): append_content_summary(), empty_context_prompt_section_is_none(), git_status_branch_surfaces_in_prompt_section(), git_status_content_type_surfaces_in_prompt_section(), latest_returns_most_recent_entry(), non_empty_context_prompt_section_has_heading(), push_onto_empty_context_length_is_one(), ring_buffer_evicts_oldest_at_max_capacity() (+2 more)

### Community 99 - "Community 99"
Cohesion: 0.14
Nodes (22): CommandOutput, execute_sandboxed(), none_policy_reports_nonzero_exit(), none_policy_runs_echo(), none_policy_timeout_fires(), permissive_policy_captures_stderr(), permissive_policy_runs_echo(), resource_exhaustion_100_rapid_calls_no_leak() (+14 more)

### Community 100 - "Community 100"
Cohesion: 0.11
Nodes (14): audio(), audio_ref_json_round_trip(), frame(), frame_ref_json_round_trip(), importance_clamped_above_one_survives_round_trip(), importance_clamped_below_zero_survives_round_trip(), json_round_trip_all_modalities_bundle(), json_round_trip_audio_only_bundle() (+6 more)

### Community 101 - "Community 101"
Cohesion: 0.21
Nodes (19): active_pane_outranks_idle_pane(), activity_never_active_pane_scores_zero_activity(), agent_distress_adds_to_score(), Attention, deterministic_tie_breaking_by_adapter_id(), error_pane_outranks_idle_pane(), error_signal_saturates_at_three_tokens(), make_active() (+11 more)

### Community 102 - "Community 102"
Cohesion: 0.14
Nodes (15): dim_mismatch_is_rejected(), empty_modality_search_returns_empty(), InMemoryVectorIndex, ModalityTable, remove_is_idempotent(), search_truncates_to_limit(), upsert_then_search_orders_by_similarity(), VectorHit (+7 more)

### Community 103 - "Community 103"
Cohesion: 0.24
Nodes (15): compute_layout_on_empty_dag_is_noop(), compute_layout_places_all_nodes(), DagViewerState, edge(), handle_click_miss_clears_selection(), handle_click_selects_node(), handle_pan_shifts_offset(), handle_zoom_scales_and_clamps() (+7 more)

### Community 104 - "Community 104"
Cohesion: 0.23
Nodes (11): add_and_retrieve_skill(), format_for_prompt_shows_skills(), list_skill_names(), load_from_memory_round_trips(), now_epoch(), record_use_increments_count(), retrieve_caps_at_top_k(), retrieve_returns_empty_for_no_match() (+3 more)

### Community 105 - "Community 105"
Cohesion: 0.2
Nodes (14): dim_mismatch_is_rejected(), DimCache, embedding_dim_from_schema(), empty_batch(), empty_modality_search_returns_empty(), ids_for_modality_tracks_inserts_and_removes(), LanceDbIndex, modalities_lists_all_tables() (+6 more)

### Community 106 - "Community 106"
Cohesion: 0.18
Nodes (10): channel_filtering(), channel_for_target(), chrono_free_timestamp(), file_mirror_to_tempdir(), install(), install_panic_hook(), level_to_verbosity(), PhantomLogger (+2 more)

### Community 107 - "Community 107"
Cohesion: 0.22
Nodes (12): ChildHandle, JoinHandle<T>, noop_waker(), NowOrNeverResult, one_for_one_rate_limit_escalates(), permanent_policy_respawns_after_clean_exit(), RestartPolicy, shutdown_cancels_all_children() (+4 more)

### Community 108 - "Community 108"
Cohesion: 0.17
Nodes (14): all_grants_every_permission(), all_tools_allowed_with_all_permissions(), check_tool_git_status_and_diff(), check_tool_list_files_uses_read_permission(), check_tool_read_file_allowed(), check_tool_run_command_denied(), check_tool_search_files_uses_read_permission(), check_tool_write_file_denied() (+6 more)

### Community 109 - "Community 109"
Cohesion: 0.2
Nodes (12): b64_round_trip_32_bytes(), BlobEnvelope, DataEncryptionKey, decode_b64_32(), dek_derivation_is_deterministic_per_bundle_id(), encode_b64_32(), envelope_bytes_round_trip(), MasterKey (+4 more)

### Community 110 - "Community 110"
Cohesion: 0.12
Nodes (6): CurrentValues, SettingsItem, SettingsKind, SettingsPanel, SettingsSection, SettingsSnapshot

### Community 111 - "Community 111"
Cohesion: 0.19
Nodes (11): after_ping_waits_for_pong(), backoff_caps_at_max(), backoff_delay(), backoff_grows_exponentially(), first_poll_requests_ping(), Heartbeat, HeartbeatAction, HeartbeatState (+3 more)

### Community 112 - "Community 112"
Cohesion: 0.25
Nodes (14): feed(), headless_term(), no_panic_all_byte_values(), no_panic_arabic_marhaba(), no_panic_combined_edge_cases(), no_panic_e_combining_acute(), no_panic_fire_skull_emoji(), no_panic_null_byte() (+6 more)

### Community 113 - "Community 113"
Cohesion: 0.18
Nodes (6): BootOrder, shutdown_guard_default_matches_new(), shutdown_guard_is_idempotent(), shutdown_order_is_reversed(), ShutdownGuard, SubsystemTier

### Community 114 - "Community 114"
Cohesion: 0.21
Nodes (12): as_uuid_round_trips(), clone_produces_equal_id(), copy_produces_equal_id(), CorrelationId, debug_includes_correlation_id_wrapper_name(), display_emits_hyphenated_uuid_format(), distinct_ids_can_coexist_in_hash_set(), eq_is_reflexive() (+4 more)

### Community 115 - "Community 115"
Cohesion: 0.25
Nodes (1): App

### Community 116 - "Community 116"
Cohesion: 0.22
Nodes (6): CrtSettings, default_settings_round_trip(), load_from_nonexistent_returns_default(), PhantomSettings, save_and_reload(), ScrollSettings

### Community 117 - "Community 117"
Cohesion: 0.2
Nodes (8): cell_w_and_h_return_components(), default_equals_fallback(), fallback_matches_legacy_assumptions(), measure_mono_empty_string_zero(), measure_mono_handles_unicode_by_char_count(), measure_mono_scales_with_cell_width(), RenderCtx, space_n_is_4n()

### Community 118 - "Community 118"
Cohesion: 0.24
Nodes (13): agent_node(), build_overlay(), build_overlay_in_progress_label_uses_higher_weight(), build_overlay_invalid_json_returns_empty(), build_overlay_maps_issue_to_matching_dag_node(), extract_crate_names(), extract_crate_names_finds_multiple_crates_in_one_body(), GhIssue (+5 more)

### Community 119 - "Community 119"
Cohesion: 0.29
Nodes (11): agent_capture_is_clone(), agent_capture_records_run(), history_append_increments_count(), history_command_and_exit_code_round_trip(), history_empty_store_is_safe(), history_recent_chronological_order(), history_recent_limit_exceeds_total(), history_survives_reopen() (+3 more)

### Community 120 - "Community 120"
Cohesion: 0.17
Nodes (1): TaintLevel

### Community 121 - "Community 121"
Cohesion: 0.22
Nodes (5): create_bind_group(), create_offscreen_texture(), crt_wgsl_source(), PostFxParams, PostFxPipeline

### Community 122 - "Community 122"
Cohesion: 0.41
Nodes (7): apply_modifies_grid_cell(), apply_out_of_bounds_is_safe(), apply_skips_spaces(), GlitchCell, KeystrokeFx, noise_hash(), trigger_and_tick_lifecycle()

### Community 123 - "Community 123"
Cohesion: 0.17
Nodes (0): 

### Community 124 - "Community 124"
Cohesion: 0.21
Nodes (9): build_error_analysis_prompt(), build_error_analysis_prompt_caps_at_5_errors(), build_error_analysis_prompt_formats_correctly(), ContentBlock, is_available(), is_available_does_not_panic(), Message, MessagesRequest (+1 more)

### Community 125 - "Community 125"
Cohesion: 0.24
Nodes (5): default_is_safe(), poll_returns_none_when_env_var_not_set(), repeated_poll_returns_none(), ShaderEvent, ShaderReloader

### Community 126 - "Community 126"
Cohesion: 0.29
Nodes (9): custom_namespace_rejected(), registry_stable_after_rejection(), valid_plugin_loads_correctly(), wasi_fd_write_rejected(), wasi_sock_open_rejected(), wasm_valid_no_wasi(), wasm_with_custom_namespace(), wasm_with_fd_write() (+1 more)

### Community 127 - "Community 127"
Cohesion: 0.29
Nodes (7): crt_global_bindings(), crt_wgsl_all_bindings_in_group0(), crt_wgsl_binding_slots_are_contiguous_0_1_2(), crt_wgsl_exactly_three_bindings(), crt_wgsl_params_uniform_at_group0_binding2(), crt_wgsl_scene_sampler_at_group0_binding1(), crt_wgsl_scene_texture_at_group0_binding0()

### Community 128 - "Community 128"
Cohesion: 0.25
Nodes (6): encode_png(), encode_png_1x1(), encode_png_produces_valid_png(), save_screenshot(), save_screenshot_writes_files(), ScreenshotMetadata

### Community 129 - "Community 129"
Cohesion: 0.33
Nodes (7): rust_ctx(), translate_clarify_for_ambiguous_input(), translate_empty_input_is_clarify_without_backend_call(), translate_malformed_backend_reply_maps_to_clarify(), translate_run_command_with_mock(), translate_search_history_with_mock(), translate_spawn_agent_with_mock()

### Community 130 - "Community 130"
Cohesion: 0.33
Nodes (1): App

### Community 131 - "Community 131"
Cohesion: 0.27
Nodes (8): build_error_triage_prompt(), build_error_triage_prompt_caps_at_3_errors(), build_error_triage_prompt_formats_correctly(), GenerateOptions, GenerateRequest, GenerateResponse, is_available(), is_available_returns_false_when_no_server()

### Community 132 - "Community 132"
Cohesion: 0.39
Nodes (6): dedup_identical_frames_detected(), embedding(), end_to_end_capture_pipeline(), open_tmp(), solid_rgba(), store_insert_and_retrieve_by_id()

### Community 133 - "Community 133"
Cohesion: 0.53
Nodes (4): disconnected_sets_have_different_roots(), singleton_is_own_root(), union_merges_components(), UnionFind

### Community 134 - "Community 134"
Cohesion: 0.5
Nodes (8): fs(), measure_text(), measure_text_doubles_with_font_size(), measure_text_empty_string_is_zero(), measure_text_max_width_forces_wrap(), measure_text_single_m_matches_renderer_cell_width(), measure_text_width_monotonic_with_length(), TextMeasurement

### Community 135 - "Community 135"
Cohesion: 0.28
Nodes (3): probe_shell_path(), resolve_desktop_path(), resolve_desktop_path_unix()

### Community 136 - "Community 136"
Cohesion: 0.32
Nodes (1): SupervisorClient

### Community 137 - "Community 137"
Cohesion: 0.29
Nodes (3): frame_mark(), frame_mark_is_callable(), ProfileScope

### Community 138 - "Community 138"
Cohesion: 0.43
Nodes (4): boot_smoke_full_invariants(), make_runtime(), runtime_new_succeeds_with_test_config(), runtime_tick_with_no_events_does_not_panic()

### Community 139 - "Community 139"
Cohesion: 0.57
Nodes (1): App

### Community 140 - "Community 140"
Cohesion: 0.29
Nodes (1): WsTransport

### Community 141 - "Community 141"
Cohesion: 0.52
Nodes (3): allows_up_to_rate(), refills_over_time(), TokenBucket

### Community 142 - "Community 142"
Cohesion: 0.53
Nodes (4): AgentPolicy, default_policy_matches_former_global_values(), policy_clone_is_independent(), policy_fields_are_independently_mutable()

### Community 143 - "Community 143"
Cohesion: 0.33
Nodes (1): GlyphClipRect

### Community 144 - "Community 144"
Cohesion: 0.33
Nodes (1): ClipRect

### Community 145 - "Community 145"
Cohesion: 0.4
Nodes (1): GpuContext

### Community 146 - "Community 146"
Cohesion: 0.83
Nodes (1): App

### Community 147 - "Community 147"
Cohesion: 0.5
Nodes (0): 

### Community 148 - "Community 148"
Cohesion: 0.67
Nodes (1): App

### Community 149 - "Community 149"
Cohesion: 0.5
Nodes (1): AgentPane

### Community 150 - "Community 150"
Cohesion: 0.5
Nodes (1): RuntimeMode

### Community 151 - "Community 151"
Cohesion: 0.5
Nodes (1): DirtyFlags

### Community 152 - "Community 152"
Cohesion: 0.5
Nodes (1): DagFile

### Community 153 - "Community 153"
Cohesion: 1.0
Nodes (1): AgentPane

### Community 154 - "Community 154"
Cohesion: 0.67
Nodes (0): 

### Community 155 - "Community 155"
Cohesion: 0.67
Nodes (0): 

### Community 156 - "Community 156"
Cohesion: 1.0
Nodes (1): App

### Community 157 - "Community 157"
Cohesion: 1.0
Nodes (1): AgentPane

### Community 158 - "Community 158"
Cohesion: 1.0
Nodes (1): String

### Community 159 - "Community 159"
Cohesion: 1.0
Nodes (1): AiAction

### Community 160 - "Community 160"
Cohesion: 1.0
Nodes (1): MockStream<F>

### Community 161 - "Community 161"
Cohesion: 1.0
Nodes (1): T

## Knowledge Gaps
- **377 isolated node(s):** `CommandType`, `GitCommand`, `CargoCommand`, `DockerCommand`, `NpmCommand` (+372 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 156`** (2 nodes): `App`, `.collect_metrics()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 157`** (2 nodes): `AgentPane`, `.build_dispatch_context()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 158`** (2 nodes): `String`, `.from()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 159`** (2 nodes): `AiAction`, `.execute()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 160`** (2 nodes): `MockStream<F>`, `.poll_next()`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 161`** (1 nodes): `T`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What connects `CommandType`, `GitCommand`, `CargoCommand` to the rest of the system?**
  _377 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Community 0` be split into smaller, more focused modules?**
  _Cohesion score 0.01 - nodes in this community are weakly interconnected._
- **Should `Community 1` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Community 2` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Community 3` be split into smaller, more focused modules?**
  _Cohesion score 0.04 - nodes in this community are weakly interconnected._
- **Should `Community 4` be split into smaller, more focused modules?**
  _Cohesion score 0.02 - nodes in this community are weakly interconnected._
- **Should `Community 5` be split into smaller, more focused modules?**
  _Cohesion score 0.03 - nodes in this community are weakly interconnected._