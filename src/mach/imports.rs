//! Dynamically linked symbolic imports

// table of tuples:
// <seg-index, seg-offset, type, symbol-library-ordinal, symbol-name, addend>
// symbol flags are undocumented

use alloc::vec::Vec;
use core::fmt::{self, Debug};
use core::ops::Range;
use scroll::{Pread, Sleb128, Uleb128};

use crate::container;
use crate::error;
use crate::mach::bind_opcodes;
use crate::mach::load_command;
use crate::mach::segment;

#[derive(Debug)]
/// Import binding information generated by running the Finite State Automaton programmed via `bind_opcodes`
struct BindInformation<'a> {
    seg_index: u8,
    seg_offset: u64,
    bind_type: u8,
    symbol_library_ordinal: u8,
    symbol_name: &'a str,
    symbol_flags: u8,
    addend: i64,
    special_dylib: u8, // seeing self = 0 assuming this means the symbol is imported from itself, because its... libSystem.B.dylib?
    is_lazy: bool,
}

impl<'a> BindInformation<'a> {
    pub fn new(is_lazy: bool) -> Self {
        let mut bind_info = BindInformation::default();
        if is_lazy {
            bind_info.is_lazy = true;
            bind_info.bind_type = bind_opcodes::BIND_TYPE_POINTER;
        }
        bind_info
    }
    pub fn is_weak(&self) -> bool {
        self.symbol_flags & bind_opcodes::BIND_SYMBOL_FLAGS_WEAK_IMPORT != 0
    }
}

impl<'a> Default for BindInformation<'a> {
    fn default() -> Self {
        BindInformation {
            seg_index: 0,
            seg_offset: 0x0,
            bind_type: 0x0,
            special_dylib: 1,
            symbol_library_ordinal: 0,
            symbol_name: "",
            symbol_flags: 0,
            addend: 0,
            is_lazy: false,
        }
    }
}

#[derive(Debug)]
/// An dynamically linked symbolic import
pub struct Import<'a> {
    /// The symbol name dyld uses to resolve this import
    pub name: &'a str,
    /// The library this symbol belongs to (thanks to two-level namespaces)
    pub dylib: &'a str,
    ///  Whether the symbol is lazily resolved or not
    pub is_lazy: bool,
    /// The offset in the binary this import is found
    pub offset: u64,
    /// The size of this import
    pub size: usize,
    /// The virtual memory address at which this import is found
    pub address: u64,
    /// The addend of this import
    pub addend: i64,
    /// Whether this import is weak
    pub is_weak: bool,
    /// The offset in the stream of bind opcodes that caused this import
    pub start_of_sequence_offset: u64,
}

impl<'a> Import<'a> {
    /// Create a new import from the import binding information in `bi`
    fn new(
        bi: &BindInformation<'a>,
        libs: &[&'a str],
        segments: &[segment::Segment],
        start_of_sequence_offset: usize,
    ) -> Import<'a> {
        let (offset, address) = {
            let segment = &segments[bi.seg_index as usize];
            (
                segment.fileoff + bi.seg_offset,
                segment.vmaddr + bi.seg_offset,
            )
        };
        let size = if bi.is_lazy { 8 } else { 0 };
        Import {
            name: bi.symbol_name,
            dylib: libs[bi.symbol_library_ordinal as usize],
            is_lazy: bi.is_lazy,
            offset,
            size,
            address,
            addend: bi.addend,
            is_weak: bi.is_weak(),
            start_of_sequence_offset: start_of_sequence_offset as u64,
        }
    }
}

/// An interpreter for mach BIND opcodes.
/// Runs on prebound (non lazy) symbols (usually dylib extern consts and extern variables),
/// and lazy symbols (usually dylib functions)
#[derive(Clone)]
pub struct BindInterpreter<'a> {
    data: &'a [u8],
    location: Range<usize>,
    lazy_location: Range<usize>,
}

impl<'a> Debug for BindInterpreter<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("BindInterpreter")
            .field("data", &"<... redacted ...>")
            .field(
                "location",
                &format_args!("{:#x}..{:#x}", self.location.start, self.location.end),
            )
            .field(
                "lazy_location",
                &format_args!(
                    "{:#x}..{:#x}",
                    self.lazy_location.start, self.lazy_location.end
                ),
            )
            .finish()
    }
}

impl<'a> BindInterpreter<'a> {
    /// Construct a new import binding interpreter from `bytes` and the load `command`
    pub fn new(bytes: &'a [u8], command: &load_command::DyldInfoCommand) -> Self {
        let get_pos = |off: u32, size: u32| -> Range<usize> {
            let start = off as usize;
            start..start.saturating_add(size as usize)
        };
        let location = get_pos(command.bind_off, command.bind_size);
        let lazy_location = get_pos(command.lazy_bind_off, command.lazy_bind_size);
        BindInterpreter {
            data: bytes,
            location,
            lazy_location,
        }
    }
    /// Return the imports in this binary
    pub fn imports(
        &self,
        libs: &[&'a str],
        segments: &[segment::Segment],
        ctx: container::Ctx,
    ) -> error::Result<Vec<Import<'a>>> {
        let mut imports = Vec::new();
        self.run(false, libs, segments, ctx, &mut imports)?;
        self.run(true, libs, segments, ctx, &mut imports)?;
        Ok(imports)
    }
    fn run(
        &self,
        is_lazy: bool,
        libs: &[&'a str],
        segments: &[segment::Segment],
        ctx: container::Ctx,
        imports: &mut Vec<Import<'a>>,
    ) -> error::Result<()> {
        use crate::mach::bind_opcodes::*;
        let location = if is_lazy {
            &self.lazy_location
        } else {
            &self.location
        };
        let mut bind_info = BindInformation::new(is_lazy);
        let mut offset = location.start;
        let mut start_of_sequence: usize = 0;
        while offset < location.end {
            let opcode = self.data.gread::<i8>(&mut offset)? as bind_opcodes::Opcode;
            // let mut input = String::new();
            // ::std::io::stdin().read_line(&mut input).unwrap();
            // println!("opcode: {} ({:#x}) offset: {:#x}\n {:?}", opcode_to_str(opcode & BIND_OPCODE_MASK), opcode, offset - location.start - 1, &bind_info);
            match opcode & BIND_OPCODE_MASK {
                // we do nothing, don't update our records, and add a new, fresh record
                BIND_OPCODE_DONE => {
                    bind_info = BindInformation::new(is_lazy);
                    start_of_sequence = offset - location.start;
                }
                BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => {
                    let symbol_library_ordinal = opcode & BIND_IMMEDIATE_MASK;
                    bind_info.symbol_library_ordinal = symbol_library_ordinal;
                }
                BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => {
                    let symbol_library_ordinal = Uleb128::read(&self.data, &mut offset)?;
                    bind_info.symbol_library_ordinal = symbol_library_ordinal as u8;
                }
                BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                    // dyld puts the immediate into the symbol_library_ordinal field...
                    let special_dylib = opcode & BIND_IMMEDIATE_MASK;
                    // Printf.printf "special_dylib: 0x%x\n" special_dylib
                    bind_info.special_dylib = special_dylib;
                }
                BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                    let symbol_flags = opcode & BIND_IMMEDIATE_MASK;
                    let symbol_name = self.data.pread::<&str>(offset)?;
                    offset += symbol_name.len() + 1; // second time this \0 caused debug woes
                    bind_info.symbol_name = symbol_name;
                    bind_info.symbol_flags = symbol_flags;
                }
                BIND_OPCODE_SET_TYPE_IMM => {
                    let bind_type = opcode & BIND_IMMEDIATE_MASK;
                    bind_info.bind_type = bind_type;
                }
                BIND_OPCODE_SET_ADDEND_SLEB => {
                    let addend = Sleb128::read(&self.data, &mut offset)?;
                    bind_info.addend = addend;
                }
                BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                    let seg_index = opcode & BIND_IMMEDIATE_MASK;
                    // dyld sets the address to the segActualLoadAddress(segIndex) + uleb128
                    // address = segActualLoadAddress(segmentIndex) + read_uleb128(p, end);
                    let seg_offset = Uleb128::read(&self.data, &mut offset)?;
                    bind_info.seg_index = seg_index;
                    bind_info.seg_offset = seg_offset;
                }
                BIND_OPCODE_ADD_ADDR_ULEB => {
                    let addr = Uleb128::read(&self.data, &mut offset)?;
                    let seg_offset = bind_info.seg_offset.wrapping_add(addr);
                    bind_info.seg_offset = seg_offset;
                }
                // record the record by placing its value into our list
                BIND_OPCODE_DO_BIND => {
                    // from dyld:
                    //      if ( address >= segmentEndAddress )
                    // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
                    // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
                    // address += sizeof(intptr_t);
                    imports.push(Import::new(&bind_info, libs, segments, start_of_sequence));
                    let seg_offset = bind_info.seg_offset.wrapping_add(ctx.size() as u64);
                    bind_info.seg_offset = seg_offset;
                }
                BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => {
                    // dyld:
                    // if ( address >= segmentEndAddress )
                    // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
                    // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
                    // address += read_uleb128(p, end) + sizeof(intptr_t);
                    // we bind the old record, then increment bind info address for the next guy, plus the ptr offset *)
                    imports.push(Import::new(&bind_info, libs, segments, start_of_sequence));
                    let addr = Uleb128::read(&self.data, &mut offset)?;
                    let seg_offset = bind_info
                        .seg_offset
                        .wrapping_add(addr)
                        .wrapping_add(ctx.size() as u64);
                    bind_info.seg_offset = seg_offset;
                }
                BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED => {
                    // dyld:
                    // if ( address >= segmentEndAddress )
                    // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
                    // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
                    // address += immediate*sizeof(intptr_t) + sizeof(intptr_t);
                    // break;
                    // similarly, we bind the old record, then perform address manipulation for the next record
                    imports.push(Import::new(&bind_info, libs, segments, start_of_sequence));
                    let scale = opcode & BIND_IMMEDIATE_MASK;
                    let size = ctx.size() as u64;
                    let seg_offset = bind_info
                        .seg_offset
                        .wrapping_add(u64::from(scale) * size)
                        .wrapping_add(size);
                    bind_info.seg_offset = seg_offset;
                }
                BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                    // dyld:
                    // count = read_uleb128(p, end);
                    // skip = read_uleb128(p, end);
                    // for (uint32_t i=0; i < count; ++i) {
                    // if ( address >= segmentEndAddress )
                    // throwBadBindingAddress(address, segmentEndAddress, segmentIndex, start, end, p);
                    // (this->*handler)(context, address, type, symbolName, symboFlags, addend, libraryOrdinal, "", &last);
                    // address += skip + sizeof(intptr_t);
                    // }
                    // break;
                    let count = Uleb128::read(&self.data, &mut offset)?;
                    let skip = Uleb128::read(&self.data, &mut offset)?;
                    let skip_plus_size = skip + ctx.size() as u64;
                    for _i in 0..count {
                        imports.push(Import::new(&bind_info, libs, segments, start_of_sequence));
                        let seg_offset = bind_info.seg_offset.wrapping_add(skip_plus_size);
                        bind_info.seg_offset = seg_offset;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}