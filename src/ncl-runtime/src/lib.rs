//! Resident runtime: value representation, symbols and packages,
//! features, GC (later), FFI (later), and the iGui shim (later).
//!
//! OS-specific code lives only inside the iGui submodule (when it
//! lands). The rest is OS-independent Rust — see MANIFESTO.md,
//! "Design values" #2.
//!
//! For function-redefinition semantics see MANIFESTO.md, "Note:
//! function redefinition and dispatch": global function cells are
//! atomically swapped, retirement machinery is reserved for orphaned
//! compiled code only.

pub mod abi;
pub mod audio;
pub mod bignum;
pub mod brk;
pub mod chars;
pub mod complex;
pub mod file_sys;
pub mod float;
pub mod format;
pub mod hot_reload;
pub mod ratio;
pub mod gc_function;
pub mod gc_string;
pub mod gc_symbol;
pub mod gc;
pub mod heap;
pub mod heap_common;
#[cfg(windows)]
pub mod igui;
pub mod mutator;
pub mod printer;
pub mod random;
pub mod stack_map;
pub mod static_area;
pub mod sym_names;
pub mod symbol;
pub mod threads;
pub mod universe;
pub mod value;
pub mod win_buffer;
pub mod win_callback;
pub mod win_ffi;
pub mod win_metadata;
pub mod win_surface;
pub mod word;

pub use abi::{
    append_shim, car_shim, cdr_shim, close_stream_shim, cons_shim, list_shim,
    delete_file_shim, eq_shim, eql_shim,
    equal_shim, error_shim,
    fboundp_shim, file_exists_shim, file_length_shim, file_position_shim,
    fmakunbound_shim, format_shim,
    gensym_shim, handler_case_shim, intern_shim,
    macro_function_shim, special_operator_p_shim,
    set_symbol_function_shim, symbol_function_shim, symbol_name_shim, symbol_package_shim,
    length_shim, lt_shim, gt_shim, le_shim, ge_shim, num_eq_shim, num_ne_shim,
    add_shim, sub_shim, mul_shim,
    boundp_shim, symbol_value_shim, set_shim, type_of_shim,
    loop_return_shim, make_array_shim, multiple_value_list_of_shim, mv_clear_shim,
    values_shim, values_list_shim,
    native_block_shim, native_loop_shim,
    return_from_shim, unwind_protect_shim,
    catch_shim, throw_shim,
    ncl_abort_pending, ncl_alloc_cons, ncl_apply, ncl_aref_generic, ncl_aset_generic,
    ncl_build_rest_list, ncl_call, ncl_car, ncl_cdr, ncl_equal, ncl_funcall,
    ncl_length, ncl_load_function, ncl_load_value, ncl_lookup_keyword,
    ncl_debug_check_cons,
    ncl_dynamic_bind, ncl_dynamic_unbind,
    ncl_make_closure, ncl_pop_root, ncl_push_root, ncl_roots_reserve, ncl_set_car,
    ncl_set_cdr, ncl_set_mv_many, ncl_set_mv_single, ncl_store_value,
    ncl_string_char, ncl_string_eq, ncl_string_set,
    vector_shim, word_hash_shim, equal_hash_shim,
    string_upcase_shim, string_downcase_shim,
    string_trim_shim, string_left_trim_shim, string_right_trim_shim,
    parse_integer_shim,
    open_append_file_shim, open_input_file_shim, open_output_file_shim,
    read_char_from_shim, read_line_shim, typep_shim, write_string_to_shim,
    NclCondition,
};

#[cfg(windows)]
pub use igui::lisp_shims::{
    begin_batch_shim, close_child_shim, emit_clear_shim, emit_draw_arc_shim,
    emit_draw_line_shim, emit_draw_text_shim, emit_draw_text_styled_shim,
    emit_fill_circle_shim,
    emit_fill_oval_shim, emit_fill_rect_shim, emit_stroke_circle_shim,
    emit_stroke_oval_shim, emit_stroke_rect_shim, igui_quit_shim,
    igui_start_shim, igui_wait_shim, log_write_shim, measure_text_shim,
    clear_event_filter_shim, discard_stashed_events_shim, filter_on_window_shim,
    next_event_for_shim, next_event_shim, open_child_shim, open_child_sized_shim, open_text_window_shim,
    set_redraw_rate_shim, unfilter_window_shim,
    set_title_shim, submit_batch_shim, text_clear_eol_shim, text_clear_eos_shim, text_clear_shim,
    text_newline_shim, text_reset_pen_shim, text_scroll_up_shim,
    text_set_cursor_shim, text_set_pen_shim, text_show_caret_shim,
    text_write_char_shim, text_write_shim,
    open_repl_window_shim, repl_output_shim, repl_error_shim, repl_pop_input_shim,
    open_doc_window_shim, doc_set_markdown_shim, doc_append_markdown_shim,
    canvas_open_shim, canvas_present_shim,
    mdi_arrange_icons_shim, mdi_cascade_shim, mdi_tile_shim,
};
pub use audio::{
    audio_abc_play_shim, audio_abc_stop_shim, audio_beep_shim, audio_blip_shim,
    audio_click_shim, audio_coin_shim, audio_hit_shim, audio_jump_shim,
    audio_master_volume_shim, audio_play_shim, audio_play_vol_shim,
    audio_start_shim, audio_stop_all_shim, audio_tone_shim, audio_zap_shim,
};
pub use printer::{format_word, format_word_aesthetic};
pub use random::random_shim;

pub use win_buffer::{
    buffer_read_wstring_shim, buffer_ref_i16_shim, buffer_ref_i32_shim, buffer_ref_i64_shim,
    buffer_ref_i8_shim, buffer_ref_ptr_shim, buffer_ref_u16_shim, buffer_ref_u32_shim,
    buffer_ref_u64_shim, buffer_ref_u8_shim, buffer_set_i16_shim, buffer_set_i32_shim,
    buffer_set_i64_shim, buffer_set_i8_shim, buffer_set_ptr_shim, buffer_set_u16_shim,
    buffer_set_u32_shim, buffer_set_u64_shim, buffer_set_u8_shim, buffer_write_wstring_shim,
    buffer_zero_shim, free_foreign_buffer_shim, make_foreign_buffer_shim,
};
pub use win_ffi::{ffi_call_shim, win32_call_shim, win32_lookup_shim};
pub use win_surface::{
    ui_execute_shim, ui_thread_id_shim, ui_thread_p_shim, windows_enabled_p_shim,
};

pub use threads::{
    allocate_critical_section_shim, atomic_cas_shim, atomic_get_shim,
    atomic_incf_shim, atomic_set_shim, condvar_broadcast_shim, get_internal_real_time_shim, test_panic_shim,
    condvar_notify_shim, condvar_wait_shim, create_thread_shim,
    current_process_id_shim, current_thread_id_shim,
    deallocate_critical_section_shim, enter_critical_section_shim,
    exit_thread_shim, join_thread_shim, leave_critical_section_shim,
    mailbox_len_shim, mailbox_receive_shim, mailbox_send_shim,
    make_atomic_counter_shim, make_condvar_shim, make_mailbox_shim,
    release_atomic_counter_shim, release_condvar_shim,
    release_mailbox_shim, resume_thread_shim, sleep_shim,
    suspend_thread_shim, terminate_thread_shim, thread_handle_shim,
    thread_safepoint_shim,
};

pub use heap::{
    CardTable, GcBit, Heap, HeapHeader, HeapType, CARD_SIZE_BYTES, CARD_SIZE_CELLS,
    MAX_OBJECT_CELLS,
};
pub use mutator::{gc_force_shim, gc_stats_shim, GcConfig, GcCoordinator, GcStats, MutatorState};
pub use stack_map::{LiveSlot, ParkedFrame, StackMap, StackMapEntry, walk_parked_frame};
pub use static_area::StaticArea;
pub use symbol::{Package, Symbol, Visibility};
pub use universe::{pkg, universe, Universe};
pub use value::{Cons, FfiBlock, Value};
pub use word::{Tag, Word, FIXNUM_MAX, FIXNUM_MIN, PAYLOAD_MASK, TAG_BITS, TAG_MASK};
