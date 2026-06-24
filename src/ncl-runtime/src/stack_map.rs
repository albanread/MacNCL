//! Stack maps for precise root walking.
//!
//! See `docs/GC.md`. Step 9 of the GC build order: the runtime side
//! of LLVM `llvm.experimental.gc.statepoint`-driven precise root
//! finding. The compiler will emit a statepoint at every safe point
//! in JIT'd code; LLVM produces a stack map describing which stack
//! slots and registers hold tagged Lisp values at that program
//! counter. At GC time the runtime walks each parked mutator's
//! frames bottom-up, looks up the stack map by PC, and visits each
//! live `Word`.
//!
//! This step lands the runtime API, the data shapes, and a frame
//! walker that's verified against manually-constructed stack maps.
//! The compiler-side emission of statepoints arrives in Phase 3,
//! and a small platform-specific shim to capture FP/PC at park time
//! lands alongside it. Until then, the existing explicit
//! `push_root` / `pop_root` API on `MutatorState` is the working
//! contract.
//!
//! The shape is deliberately minimal — just enough that when the
//! compiler arrives, plugging it in is a small connection job, not
//! a redesign.

use std::collections::HashMap;

// -- LiveSlot ----------------------------------------------------------------

/// A single live root location at one safe point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveSlot {
    /// On the parked thread's stack, at `frame_pointer + offset`.
    /// The compiler may use a positive or negative offset; a small
    /// signed integer is enough for any reasonable frame.
    FpOffset(i32),
    /// In the parked thread's saved register file, at the named
    /// general-purpose register index. The mutator's `park`
    /// captures the live registers into a side buffer; this index
    /// is into that buffer.
    SavedRegister(u8),
}

// -- StackMapEntry -----------------------------------------------------------

/// All live roots at a single safe-point PC.
#[derive(Debug, Clone)]
pub struct StackMapEntry {
    /// The exact program-counter value (machine address) of the
    /// safe point. Looked up in the global `StackMap` after a
    /// mutator parks.
    pub pc: u64,
    /// Live root slots at this safe point.
    pub slots: Vec<LiveSlot>,
}

// -- StackMap ----------------------------------------------------------------

/// Collection of stack-map entries indexed by PC. Built up by the
/// JIT as it emits code; queried by the GC during stop-the-world.
#[derive(Debug, Default)]
pub struct StackMap {
    entries: HashMap<u64, StackMapEntry>,
}

impl StackMap {
    pub fn new() -> StackMap { StackMap::default() }

    pub fn register(&mut self, entry: StackMapEntry) {
        self.entries.insert(entry.pc, entry);
    }

    pub fn lookup(&self, pc: u64) -> Option<&StackMapEntry> {
        self.entries.get(&pc)
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

// -- ParkedFrame -------------------------------------------------------------

/// State captured by a mutator at `park()` time, sufficient for the
/// GC to walk its frame. For step 9 this is data-only — the actual
/// FP/PC capture from CPU state needs a small platform-specific
/// shim that lands with Phase 3 (`__readfsbase` / inline asm /
/// frame-pointer reads via Rust intrinsics).
#[derive(Debug)]
pub struct ParkedFrame {
    /// Frame pointer of the parked frame.
    pub fp: usize,
    /// PC at the safe point — used to look up the stack map entry.
    pub pc: u64,
    /// Saved register file. Up to 16 GPRs is enough for both x86-64
    /// (16 GPRs) and aarch64 (32 GPRs — we'd grow this if we need
    /// more slots there). Index by `LiveSlot::SavedRegister`.
    pub saved_regs: [u64; 16],
}

impl ParkedFrame {
    pub fn new(fp: usize, pc: u64) -> ParkedFrame {
        ParkedFrame { fp, pc, saved_regs: [0; 16] }
    }
}

// -- Walking -----------------------------------------------------------------

/// Walk a single parked frame and call `visit` on each live root
/// slot's address. Returns `Some(n_visited)` when an entry was
/// found, `None` when no entry matches the PC (caller may fall
/// back to conservative scanning during bring-up).
///
/// `visit` receives a `*mut u64` pointing AT the slot itself. It
/// may read the slot to extract a `Word`, and write back if the
/// slot is updated by a forwarding pointer.
pub fn walk_parked_frame(
    frame: &mut ParkedFrame,
    stack_map: &StackMap,
    mut visit: impl FnMut(*mut u64),
) -> Option<usize> {
    let entry = stack_map.lookup(frame.pc)?;
    let mut count = 0;
    for slot in &entry.slots {
        match *slot {
            LiveSlot::FpOffset(off) => {
                let addr = frame.fp.wrapping_add_signed(off as isize) as *mut u64;
                visit(addr);
                count += 1;
            }
            LiveSlot::SavedRegister(idx) => {
                let addr = &mut frame.saved_regs[idx as usize] as *mut u64;
                visit(addr);
                count += 1;
            }
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word::{Tag, Word};

    #[test]
    fn empty_map_returns_none() {
        let map = StackMap::new();
        let mut frame = ParkedFrame::new(0, 100);
        let r = walk_parked_frame(&mut frame, &map, |_| panic!("should not visit"));
        assert!(r.is_none());
    }

    #[test]
    fn unknown_pc_returns_none() {
        let mut map = StackMap::new();
        map.register(StackMapEntry { pc: 100, slots: vec![] });
        let mut frame = ParkedFrame::new(0, 999);
        let r = walk_parked_frame(&mut frame, &map, |_| panic!("should not visit"));
        assert!(r.is_none());
    }

    #[test]
    fn walks_fp_offset_slots() {
        // Simulate a stack frame with three Words at known offsets.
        // The frame is a fixed-size array on our local stack; we use
        // its address as the "frame pointer" in the parked frame.
        let mut frame_storage: [u64; 8] = [0; 8];
        frame_storage[2] = Word::fixnum(11).raw();
        frame_storage[5] = Word::fixnum(22).raw();
        frame_storage[7] = Word::fixnum(33).raw();
        let fp = frame_storage.as_ptr() as usize;

        let mut map = StackMap::new();
        map.register(StackMapEntry {
            pc: 0xDEADBEEF,
            slots: vec![
                LiveSlot::FpOffset(2 * 8),
                LiveSlot::FpOffset(5 * 8),
                LiveSlot::FpOffset(7 * 8),
            ],
        });

        let mut frame = ParkedFrame::new(fp, 0xDEADBEEF);
        let mut visited = Vec::new();
        let n = walk_parked_frame(&mut frame, &map, |addr| {
            visited.push(unsafe { Word::from_raw(*addr) });
        }).unwrap();

        assert_eq!(n, 3);
        assert_eq!(visited.len(), 3);
        assert_eq!(visited[0].as_fixnum(), Some(11));
        assert_eq!(visited[1].as_fixnum(), Some(22));
        assert_eq!(visited[2].as_fixnum(), Some(33));
    }

    #[test]
    fn walks_saved_register_slots() {
        let mut map = StackMap::new();
        map.register(StackMapEntry {
            pc: 1,
            slots: vec![LiveSlot::SavedRegister(3), LiveSlot::SavedRegister(7)],
        });
        let mut frame = ParkedFrame::new(0, 1);
        frame.saved_regs[3] = Word::fixnum(100).raw();
        frame.saved_regs[7] = Word::fixnum(200).raw();

        let mut seen = Vec::new();
        let n = walk_parked_frame(&mut frame, &map, |addr| {
            seen.push(unsafe { Word::from_raw(*addr) });
        }).unwrap();

        assert_eq!(n, 2);
        assert_eq!(seen[0].as_fixnum(), Some(100));
        assert_eq!(seen[1].as_fixnum(), Some(200));
    }

    #[test]
    fn visit_can_update_slot_in_place() {
        // The whole point: a forwarding-pointer update writes back.
        let mut frame_storage: [u64; 4] = [0; 4];
        frame_storage[1] = Word::fixnum(0).raw();
        let fp = frame_storage.as_ptr() as usize;

        let mut map = StackMap::new();
        map.register(StackMapEntry {
            pc: 1,
            slots: vec![LiveSlot::FpOffset(8)], // cell index 1
        });
        let mut frame = ParkedFrame::new(fp, 1);
        walk_parked_frame(&mut frame, &map, |addr| {
            // Pretend we're a forwarding update — replace the Word.
            unsafe { *addr = Word::fixnum(42).raw(); }
        });

        // The original slot was mutated.
        assert_eq!(unsafe { Word::from_raw(frame_storage[1]) }.as_fixnum(), Some(42));
    }

    #[test]
    fn negative_fp_offset_works() {
        // Compilers often place locals at negative FP offsets.
        // Build a frame with a "below FP" slot.
        let mut frame_storage: [u64; 8] = [0; 8];
        frame_storage[1] = Word::fixnum(77).raw();
        // Treat cell 4 as the FP — slot at offset -3*8 = cell 1.
        let fp = unsafe { frame_storage.as_ptr().add(4) } as usize;

        let mut map = StackMap::new();
        map.register(StackMapEntry {
            pc: 1,
            slots: vec![LiveSlot::FpOffset(-(3 * 8))],
        });
        let mut frame = ParkedFrame::new(fp, 1);
        let mut seen = None;
        walk_parked_frame(&mut frame, &map, |addr| {
            seen = Some(unsafe { Word::from_raw(*addr) });
        });
        assert_eq!(seen.unwrap().as_fixnum(), Some(77));
    }

    #[test]
    fn mixed_slots_visited_in_order() {
        let mut frame_storage: [u64; 4] = [0; 4];
        frame_storage[2] = Word::fixnum(11).raw();
        let fp = frame_storage.as_ptr() as usize;

        let mut map = StackMap::new();
        map.register(StackMapEntry {
            pc: 1,
            slots: vec![
                LiveSlot::FpOffset(2 * 8),
                LiveSlot::SavedRegister(5),
            ],
        });
        let mut frame = ParkedFrame::new(fp, 1);
        frame.saved_regs[5] = Word::fixnum(22).raw();

        let mut seen = Vec::new();
        walk_parked_frame(&mut frame, &map, |addr| {
            seen.push(unsafe { Word::from_raw(*addr) });
        });
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].as_fixnum(), Some(11));
        assert_eq!(seen[1].as_fixnum(), Some(22));
    }

    #[test]
    fn stack_map_basic_ops() {
        let mut m = StackMap::new();
        assert!(m.is_empty());
        m.register(StackMapEntry { pc: 1, slots: vec![] });
        m.register(StackMapEntry { pc: 2, slots: vec![LiveSlot::FpOffset(0)] });
        assert_eq!(m.len(), 2);
        assert!(m.lookup(1).is_some());
        assert!(m.lookup(2).is_some());
        assert!(m.lookup(3).is_none());
        let _ = Tag::Cons; // silence import
    }
}
