use std::{
    collections::{btree_map::Entry, hash_map, BTreeMap, HashMap},
    io::Cursor,
    path::Path,
};

use anyhow::{anyhow, bail, ensure, Context, Result};
use cwdemangle::demangle;
use flagset::Flags;
use indexmap::IndexMap;
use object::{
    elf,
    elf::{SHF_ALLOC, SHF_EXECINSTR, SHF_WRITE, SHT_NOBITS, SHT_PROGBITS},
    write::{
        elf::{ProgramHeader, Rel, SectionHeader, SectionIndex, SymbolIndex},
        StringId,
    },
    Architecture, Endianness, Object, ObjectKind, ObjectSection, ObjectSymbol, Relocation,
    RelocationKind, RelocationTarget, Section, SectionKind, Symbol, SymbolKind, SymbolScope,
    SymbolSection,
};

use crate::{
    obj::{
        ObjArchitecture, ObjInfo, ObjKind, ObjReloc, ObjRelocKind, ObjSection, ObjSectionKind,
        ObjSymbol, ObjSymbolFlagSet, ObjSymbolFlags, ObjSymbolKind,
    },
    util::{
        dwarf::{
            process_address, process_type, read_debug_section, type_string, ud_type,
            ud_type_string, AttributeKind, TagKind, TypeKind,
        },
        file::map_file,
    },
};
use crate::util::nested::NestedVec;

enum BoundaryState {
    /// Looking for a file symbol, any section symbols are queued
    LookForFile(Vec<(u64, String)>),
    /// Looking for section symbols
    LookForSections(String),
    /// Done with files and sections
    FilesEnded,
}

const ENABLE_DWARF: bool = false;

pub fn process_elf<P: AsRef<Path>>(path: P) -> Result<ObjInfo> {
    let mmap = map_file(path)?;
    let obj_file = object::read::File::parse(&*mmap)?;
    let architecture = match obj_file.architecture() {
        Architecture::PowerPc => ObjArchitecture::PowerPc,
        arch => bail!("Unexpected architecture: {arch:?}"),
    };
    ensure!(obj_file.endianness() == Endianness::Big, "Expected big endian");
    let kind = match obj_file.kind() {
        ObjectKind::Executable => ObjKind::Executable,
        ObjectKind::Relocatable => ObjKind::Relocatable,
        kind => bail!("Unexpected ELF type: {kind:?}"),
    };

    if ENABLE_DWARF {
        if let Some(debug_section) = obj_file.section_by_name(".debug") {
            if debug_section.size() > 0 {
                load_debug_section(&obj_file, debug_section)?;
            }
        }
    }

    let mut obj_name = String::new();
    let mut stack_address: Option<u32> = None;
    let mut stack_end: Option<u32> = None;
    let mut db_stack_addr: Option<u32> = None;
    let mut arena_lo: Option<u32> = None;
    let mut arena_hi: Option<u32> = None;
    let mut sda_base: Option<u32> = None;
    let mut sda2_base: Option<u32> = None;

    let mut sections: Vec<ObjSection> = vec![];
    let mut section_indexes: Vec<Option<usize>> = vec![];
    for section in obj_file.sections() {
        let section_kind = match section.kind() {
            SectionKind::Text => ObjSectionKind::Code,
            SectionKind::Data => ObjSectionKind::Data,
            SectionKind::ReadOnlyData => ObjSectionKind::ReadOnlyData,
            SectionKind::UninitializedData => ObjSectionKind::Bss,
            _ => {
                section_indexes.push(None);
                continue;
            }
        };
        section_indexes.push(Some(sections.len()));
        sections.push(ObjSection {
            name: section.name()?.to_string(),
            kind: section_kind,
            address: section.address(),
            size: section.size(),
            data: section.uncompressed_data()?.to_vec(),
            align: section.align(),
            index: sections.len(),
            elf_index: section.index().0,
            relocations: vec![],
            original_address: 0, // TODO load from abs symbol
            file_offset: section.file_range().map(|(v, _)| v).unwrap_or_default(),
            section_known: true,
        });
    }

    let mut symbols: Vec<ObjSymbol> = vec![];
    let mut symbol_indexes: Vec<Option<usize>> = vec![];
    let mut section_starts = IndexMap::<String, Vec<(u64, String)>>::new();
    let mut name_to_index = HashMap::<String, usize>::new(); // for resolving duplicate names
    let mut boundary_state = BoundaryState::LookForFile(Default::default());

    for symbol in obj_file.symbols() {
        // Locate linker-generated symbols
        let symbol_name = symbol.name()?;
        match symbol_name {
            "_stack_addr" => stack_address = Some(symbol.address() as u32),
            "_stack_end" => stack_end = Some(symbol.address() as u32),
            "_db_stack_addr" => db_stack_addr = Some(symbol.address() as u32),
            "__ArenaLo" => arena_lo = Some(symbol.address() as u32),
            "__ArenaHi" => arena_hi = Some(symbol.address() as u32),
            "_SDA_BASE_" => sda_base = Some(symbol.address() as u32),
            "_SDA2_BASE_" => sda2_base = Some(symbol.address() as u32),
            _ => {}
        };

        // MWCC has file symbol first, then sections
        // GCC has section symbols first, then file
        match symbol.kind() {
            SymbolKind::File => {
                let mut file_name = symbol_name.to_string();
                // Try to exclude precompiled header symbols
                // Make configurable eventually
                if file_name == "Precompiled.cpp"
                    || file_name == "stdafx.cpp"
                    || file_name.ends_with(".h")
                    || file_name.starts_with("Pch.")
                    || file_name.contains("precompiled_")
                    || file_name.contains("Precompiled")
                    || file_name.contains(".pch")
                    || file_name.contains("_PCH.")
                {
                    symbol_indexes.push(None);
                    continue;
                }
                if kind == ObjKind::Relocatable {
                    obj_name = file_name.clone();
                }
                let sections = match section_starts.entry(file_name.clone()) {
                    indexmap::map::Entry::Occupied(_) => {
                        let index = match name_to_index.entry(file_name.clone()) {
                            hash_map::Entry::Occupied(e) => e.into_mut(),
                            hash_map::Entry::Vacant(e) => e.insert(0),
                        };
                        *index += 1;
                        let new_name = format!("{}_{}", file_name, index);
                        // log::info!("Renaming {} to {}", file_name, new_name);
                        file_name = new_name.clone();
                        match section_starts.entry(new_name.clone()) {
                            indexmap::map::Entry::Occupied(_) => {
                                bail!("Duplicate filename '{}'", new_name)
                            }
                            indexmap::map::Entry::Vacant(e) => e.insert(Default::default()),
                        }
                    }
                    indexmap::map::Entry::Vacant(e) => e.insert(Default::default()),
                };
                match &mut boundary_state {
                    BoundaryState::LookForFile(queue) => {
                        if queue.is_empty() {
                            boundary_state = BoundaryState::LookForSections(file_name);
                        } else {
                            // Clears queue
                            sections.append(queue);
                        }
                    }
                    BoundaryState::LookForSections(_) => {
                        boundary_state = BoundaryState::LookForSections(file_name);
                    }
                    BoundaryState::FilesEnded => {
                        log::warn!("File symbol after files ended: '{}'", file_name);
                    }
                }
            }
            SymbolKind::Section => {
                let section_index = symbol
                    .section_index()
                    .ok_or_else(|| anyhow!("Section symbol without section"))?;
                let section = obj_file.section_by_index(section_index)?;
                let section_name = section.name()?.to_string();
                match &mut boundary_state {
                    BoundaryState::LookForFile(queue) => {
                        queue.push((symbol.address(), section_name));
                    }
                    BoundaryState::LookForSections(file_name) => {
                        if section_indexes[section_index.0].is_some() {
                            let sections = section_starts
                                .get_mut(file_name)
                                .ok_or_else(|| anyhow!("Failed to create entry"))?;
                            sections.push((symbol.address(), section_name));
                        }
                    }
                    BoundaryState::FilesEnded => {
                        log::warn!(
                            "Section symbol after files ended: {} @ {:#010X}",
                            section_name,
                            symbol.address()
                        );
                    }
                }
            }
            _ => match symbol.section() {
                // Linker generated symbols indicate the end
                SymbolSection::Absolute => {
                    boundary_state = BoundaryState::FilesEnded;
                }
                SymbolSection::Section(section_index) => match &mut boundary_state {
                    BoundaryState::LookForFile(_) => {}
                    BoundaryState::LookForSections(file_name) => {
                        if section_indexes[section_index.0].is_some() {
                            let sections = section_starts
                                .get_mut(file_name)
                                .ok_or_else(|| anyhow!("Failed to create entry"))?;
                            let section = obj_file.section_by_index(section_index)?;
                            let section_name = section.name()?;
                            if let Some((addr, _)) = sections
                                .iter_mut()
                                .find(|(addr, name)| *addr == 0 && name == section_name)
                            {
                                // If the section symbol had address 0, determine address
                                // from first symbol within that section.
                                *addr = symbol.address();
                            } else if !sections.iter().any(|(_, name)| name == section_name) {
                                // Otherwise, if there was no section symbol, assume this
                                // symbol indicates the section address.
                                sections.push((symbol.address(), section_name.to_string()));
                            }
                        }
                    }
                    BoundaryState::FilesEnded => {}
                },
                SymbolSection::Undefined => {}
                _ => bail!("Unsupported symbol section type {symbol:?}"),
            },
        }

        // Generate symbols
        if matches!(symbol.kind(), SymbolKind::Null | SymbolKind::File)
            || matches!(symbol.section_index(), Some(idx) if section_indexes[idx.0] == None)
        {
            symbol_indexes.push(None);
            continue;
        }
        symbol_indexes.push(Some(symbols.len()));
        symbols.push(to_obj_symbol(&obj_file, &symbol, &section_indexes)?);
    }

    let mut link_order = Vec::<String>::new();
    let mut splits = BTreeMap::<u32, Vec<String>>::new();
    if kind == ObjKind::Executable {
        // Link order is trivially deduced
        for file_name in section_starts.keys() {
            link_order.push(file_name.clone());
        }

        // Create a map of address -> file splits
        for (file_name, sections) in section_starts {
            for (address, _) in sections {
                splits.nested_push(address as u32, file_name.clone());
            }
        }

        // TODO rebuild common symbols
    }

    for (section_idx, section) in obj_file.sections().enumerate() {
        let out_section = match section_indexes[section_idx].and_then(|idx| sections.get_mut(idx)) {
            Some(s) => s,
            None => continue,
        };
        // Generate relocations
        for (address, reloc) in section.relocations() {
            out_section.relocations.push(to_obj_reloc(
                &obj_file,
                &symbol_indexes,
                &out_section.data,
                address,
                reloc,
            )?);
        }
    }

    Ok(ObjInfo {
        module_id: 0,
        kind,
        architecture,
        name: obj_name,
        symbols,
        sections,
        entry: obj_file.entry(),
        sda2_base,
        sda_base,
        stack_address,
        stack_end,
        db_stack_addr,
        arena_lo,
        arena_hi,
        splits,
        named_sections: Default::default(),
        link_order,
        known_functions: Default::default(),
        unresolved_relocations: vec![],
    })
}

pub fn write_elf(obj: &ObjInfo) -> Result<Vec<u8>> {
    let mut out_data = Vec::new();
    let mut writer = object::write::elf::Writer::new(Endianness::Big, false, &mut out_data);

    struct OutSection {
        index: SectionIndex,
        rela_index: Option<SectionIndex>,
        offset: usize,
        rela_offset: usize,
        name: StringId,
        rela_name: Option<StringId>,
    }
    struct OutSymbol {
        index: SymbolIndex,
        sym: object::write::elf::Sym,
    }

    writer.reserve_null_section_index();
    let mut out_sections: Vec<OutSection> = Vec::with_capacity(obj.sections.len());
    for section in &obj.sections {
        let name = writer.add_section_name(section.name.as_bytes());
        let index = writer.reserve_section_index();
        out_sections.push(OutSection {
            index,
            rela_index: None,
            offset: 0,
            rela_offset: 0,
            name,
            rela_name: None,
        });
    }
    let mut rela_names: Vec<String> = vec![Default::default(); obj.sections.len()];
    for ((section, out_section), rela_name) in
        obj.sections.iter().zip(&mut out_sections).zip(&mut rela_names)
    {
        if !section.relocations.is_empty() {
            *rela_name = format!(".rela{}", section.name);
            out_section.rela_name = Some(writer.add_section_name(rela_name.as_bytes()));
            out_section.rela_index = Some(writer.reserve_section_index());
        }
    }
    let symtab = writer.reserve_symtab_section_index();
    writer.reserve_shstrtab_section_index();
    writer.reserve_strtab_section_index();

    // Add symbols
    let mut out_symbols: Vec<OutSymbol> = Vec::with_capacity(obj.symbols.len());
    let mut symbol_offset = 0;
    let mut num_local = 0;
    if !obj.name.is_empty() {
        // Add file symbol
        let name_index = writer.add_string(obj.name.as_bytes());
        let index = writer.reserve_symbol_index(None);
        out_symbols.push(OutSymbol {
            index,
            sym: object::write::elf::Sym {
                name: Some(name_index),
                section: None,
                st_info: {
                    let st_type = elf::STT_FILE;
                    let st_bind = elf::STB_GLOBAL;
                    (st_bind << 4) + st_type
                },
                st_other: elf::STV_DEFAULT,
                st_shndx: elf::SHN_ABS,
                st_value: 0,
                st_size: 0,
            },
        });
        symbol_offset += 1;
    }
    for symbol in &obj.symbols {
        let section_index = symbol.section.and_then(|idx| out_sections.get(idx)).map(|s| s.index);
        let index = writer.reserve_symbol_index(section_index);
        let name_index = if symbol.name.is_empty() {
            None
        } else {
            Some(writer.add_string(symbol.name.as_bytes()))
        };
        let sym = object::write::elf::Sym {
            name: name_index,
            section: section_index,
            st_info: {
                let st_type = match symbol.kind {
                    ObjSymbolKind::Unknown => elf::STT_NOTYPE,
                    ObjSymbolKind::Function => elf::STT_FUNC,
                    ObjSymbolKind::Object => {
                        if symbol.flags.0.contains(ObjSymbolFlags::Common) {
                            elf::STT_COMMON
                        } else {
                            elf::STT_OBJECT
                        }
                    }
                    ObjSymbolKind::Section => elf::STT_SECTION,
                };
                let st_bind = if symbol.flags.0.contains(ObjSymbolFlags::Weak) {
                    elf::STB_WEAK
                } else if symbol.flags.0.contains(ObjSymbolFlags::Local) {
                    elf::STB_LOCAL
                } else {
                    elf::STB_GLOBAL
                };
                (st_bind << 4) + st_type
            },
            st_other: if symbol.flags.0.contains(ObjSymbolFlags::Hidden) {
                elf::STV_HIDDEN
            } else {
                elf::STV_DEFAULT
            },
            st_shndx: if section_index.is_some() {
                0
            } else if symbol.address != 0 {
                elf::SHN_ABS
            } else {
                elf::SHN_UNDEF
            },
            st_value: symbol.address,
            st_size: symbol.size,
        };
        if sym.st_info >> 4 == elf::STB_LOCAL {
            num_local = writer.symbol_count();
        }
        out_symbols.push(OutSymbol { index, sym });
    }

    writer.reserve_file_header();

    if obj.kind == ObjKind::Executable {
        writer.reserve_program_headers(obj.sections.len() as u32);
    }

    for (section, out_section) in obj.sections.iter().zip(&mut out_sections) {
        match section.kind {
            ObjSectionKind::Code | ObjSectionKind::Data | ObjSectionKind::ReadOnlyData => {}
            ObjSectionKind::Bss => continue,
        }
        ensure!(section.data.len() as u64 == section.size, "Mismatched section size");
        out_section.offset = writer.reserve(section.data.len(), 32);
    }

    writer.reserve_shstrtab();
    writer.reserve_strtab();
    writer.reserve_symtab();

    for (section, out_section) in obj.sections.iter().zip(&mut out_sections) {
        if section.relocations.is_empty() {
            continue;
        }
        out_section.rela_offset = writer.reserve_relocations(section.relocations.len(), true);
    }

    writer.reserve_section_headers();

    writer.write_file_header(&object::write::elf::FileHeader {
        os_abi: elf::ELFOSABI_SYSV,
        abi_version: 0,
        e_type: match obj.kind {
            ObjKind::Executable => elf::ET_EXEC,
            ObjKind::Relocatable => elf::ET_REL,
        },
        e_machine: elf::EM_PPC,
        e_entry: obj.entry,
        e_flags: elf::EF_PPC_EMB,
    })?;

    if obj.kind == ObjKind::Executable {
        writer.write_align_program_headers();
        for (section, out_section) in obj.sections.iter().zip(&out_sections) {
            writer.write_program_header(&ProgramHeader {
                p_type: elf::PT_LOAD,
                p_flags: match section.kind {
                    ObjSectionKind::Code => elf::PF_R | elf::PF_X,
                    ObjSectionKind::Data | ObjSectionKind::Bss => elf::PF_R | elf::PF_W,
                    ObjSectionKind::ReadOnlyData => elf::PF_R,
                },
                p_offset: out_section.offset as u64,
                p_vaddr: section.address,
                p_paddr: 0,
                p_filesz: match section.kind {
                    ObjSectionKind::Bss => 0,
                    _ => section.size,
                },
                p_memsz: section.size,
                p_align: 32,
            });
        }
    }

    for (section, out_section) in obj.sections.iter().zip(&out_sections) {
        if section.kind == ObjSectionKind::Bss {
            continue;
        }
        writer.write_align(32);
        debug_assert_eq!(writer.len(), out_section.offset);
        writer.write(&section.data);
    }

    writer.write_shstrtab();
    writer.write_strtab();

    writer.write_null_symbol();
    for out_symbol in &out_symbols {
        writer.write_symbol(&out_symbol.sym);
    }

    for (section, out_section) in obj.sections.iter().zip(&out_sections) {
        if section.relocations.is_empty() {
            continue;
        }
        writer.write_align_relocation();
        debug_assert_eq!(writer.len(), out_section.rela_offset);
        for reloc in &section.relocations {
            let mut r_offset = reloc.address;
            let r_type = match reloc.kind {
                ObjRelocKind::Absolute => {
                    if r_offset & 3 == 0 {
                        elf::R_PPC_ADDR32
                    } else {
                        elf::R_PPC_UADDR32
                    }
                }
                ObjRelocKind::PpcAddr16Hi => {
                    r_offset = (r_offset & !3) + 2;
                    elf::R_PPC_ADDR16_HI
                }
                ObjRelocKind::PpcAddr16Ha => {
                    r_offset = (r_offset & !3) + 2;
                    elf::R_PPC_ADDR16_HA
                }
                ObjRelocKind::PpcAddr16Lo => {
                    r_offset = (r_offset & !3) + 2;
                    elf::R_PPC_ADDR16_LO
                }
                ObjRelocKind::PpcRel24 => {
                    r_offset = r_offset & !3;
                    elf::R_PPC_REL24
                }
                ObjRelocKind::PpcRel14 => {
                    r_offset = r_offset & !3;
                    elf::R_PPC_REL14
                }
                ObjRelocKind::PpcEmbSda21 => {
                    r_offset = (r_offset & !3) + 2;
                    elf::R_PPC_EMB_SDA21
                }
            };
            writer.write_relocation(true, &Rel {
                r_offset,
                r_sym: (reloc.target_symbol + symbol_offset + 1) as u32,
                r_type,
                r_addend: reloc.addend,
            });
        }
    }

    writer.write_null_section_header();
    for (section, out_section) in obj.sections.iter().zip(&out_sections) {
        writer.write_section_header(&SectionHeader {
            name: Some(out_section.name),
            sh_type: match section.kind {
                ObjSectionKind::Code | ObjSectionKind::Data | ObjSectionKind::ReadOnlyData => {
                    SHT_PROGBITS
                }
                ObjSectionKind::Bss => SHT_NOBITS,
            },
            sh_flags: match section.kind {
                ObjSectionKind::Code => SHF_ALLOC | SHF_EXECINSTR,
                ObjSectionKind::Data | ObjSectionKind::Bss => SHF_ALLOC | SHF_WRITE,
                ObjSectionKind::ReadOnlyData => SHF_ALLOC,
            } as u64,
            sh_addr: section.address,
            sh_offset: out_section.offset as u64,
            sh_size: section.size,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: section.align,
            sh_entsize: 0, // TODO?
        });
    }
    for (section, out_section) in obj.sections.iter().zip(&out_sections) {
        let Some(rela_name) = out_section.rela_name else {
            continue;
        };
        writer.write_relocation_section_header(
            rela_name,
            out_section.index,
            symtab,
            out_section.rela_offset,
            section.relocations.len(),
            true,
        );
    }
    writer.write_symtab_section_header(num_local);
    writer.write_shstrtab_section_header();
    writer.write_strtab_section_header();

    debug_assert_eq!(writer.reserved_len(), writer.len());
    Ok(out_data)
}

fn to_obj_symbol(
    obj_file: &object::File<'_>,
    symbol: &Symbol<'_, '_>,
    section_indexes: &[Option<usize>],
) -> Result<ObjSymbol> {
    let section = match symbol.section_index() {
        Some(idx) => Some(obj_file.section_by_index(idx)?),
        None => None,
    };
    let name = match symbol.kind() {
        SymbolKind::Section => match &section {
            Some(section) => section.name()?,
            _ => bail!("Section symbol without section"),
        },
        _ => symbol.name()?,
    };
    ensure!(!name.is_empty(), "Empty symbol name");
    let mut flags = ObjSymbolFlagSet(ObjSymbolFlags::none());
    if symbol.is_global() {
        flags = ObjSymbolFlagSet(flags.0 | ObjSymbolFlags::Global);
    }
    if symbol.is_local() {
        flags = ObjSymbolFlagSet(flags.0 | ObjSymbolFlags::Local);
    }
    if symbol.is_common() {
        flags = ObjSymbolFlagSet(flags.0 | ObjSymbolFlags::Common);
    }
    if symbol.is_weak() {
        flags = ObjSymbolFlagSet(flags.0 | ObjSymbolFlags::Weak);
    }
    if symbol.scope() == SymbolScope::Linkage {
        flags = ObjSymbolFlagSet(flags.0 | ObjSymbolFlags::Hidden);
    }
    let section_idx = section.as_ref().and_then(|section| section_indexes[section.index().0]);
    Ok(ObjSymbol {
        name: name.to_string(),
        demangled_name: demangle(name, &Default::default()),
        address: symbol.address(),
        section: section_idx,
        size: symbol.size(),
        size_known: true,
        flags,
        kind: match symbol.kind() {
            SymbolKind::Text => ObjSymbolKind::Function,
            SymbolKind::Data => ObjSymbolKind::Object,
            SymbolKind::Unknown => ObjSymbolKind::Unknown,
            SymbolKind::Section => ObjSymbolKind::Section,
            _ => bail!("Unsupported symbol kind: {:?}", symbol.kind()),
        },
    })
}

fn to_obj_reloc(
    obj_file: &object::File<'_>,
    symbol_indexes: &[Option<usize>],
    section_data: &[u8],
    address: u64,
    reloc: Relocation,
) -> Result<ObjReloc> {
    let reloc_kind = match reloc.kind() {
        RelocationKind::Absolute => ObjRelocKind::Absolute,
        RelocationKind::Elf(kind) => match kind {
            elf::R_PPC_ADDR16_LO => ObjRelocKind::PpcAddr16Lo,
            elf::R_PPC_ADDR16_HI => ObjRelocKind::PpcAddr16Hi,
            elf::R_PPC_ADDR16_HA => ObjRelocKind::PpcAddr16Ha,
            elf::R_PPC_REL24 => ObjRelocKind::PpcRel24,
            elf::R_PPC_REL14 => ObjRelocKind::PpcRel14,
            elf::R_PPC_EMB_SDA21 => ObjRelocKind::PpcEmbSda21,
            _ => bail!("Unhandled PPC relocation type: {kind}"),
        },
        _ => bail!("Unhandled relocation type: {:?}", reloc.kind()),
    };
    let symbol = match reloc.target() {
        RelocationTarget::Symbol(idx) => {
            obj_file.symbol_by_index(idx).context("Failed to locate relocation target symbol")?
        }
        _ => bail!("Unhandled relocation target: {:?}", reloc.target()),
    };
    let target_symbol = symbol_indexes[symbol.index().0]
        .ok_or_else(|| anyhow!("Relocation against stripped symbol: {symbol:?}"))?;
    let addend = match symbol.kind() {
        SymbolKind::Text | SymbolKind::Data | SymbolKind::Unknown => Ok(reloc.addend()),
        SymbolKind::Section => {
            let addend = if reloc.has_implicit_addend() {
                let addend = u32::from_be_bytes(
                    section_data[address as usize..address as usize + 4].try_into()?,
                ) as i64;
                match reloc_kind {
                    ObjRelocKind::Absolute => addend,
                    _ => bail!("Unsupported implicit relocation type {reloc_kind:?}"),
                }
            } else {
                reloc.addend()
            };
            ensure!(addend >= 0, "Negative addend in section reloc: {addend}");
            Ok(addend)
        }
        _ => Err(anyhow!("Unhandled relocation symbol type {:?}", symbol.kind())),
    }?;
    let address = address & !3; // TODO hack: round down for instruction
    let reloc_data = ObjReloc { kind: reloc_kind, address, target_symbol, addend };
    Ok(reloc_data)
}

fn load_debug_section(obj_file: &object::File<'_>, debug_section: Section) -> Result<()> {
    let mut data = debug_section.uncompressed_data()?.into_owned();

    // Apply relocations to data
    for (addr, reloc) in debug_section.relocations() {
        match reloc.kind() {
            RelocationKind::Absolute | RelocationKind::Elf(elf::R_PPC_UADDR32) => {
                let target = match reloc.target() {
                    RelocationTarget::Symbol(symbol_idx) => {
                        let symbol = obj_file.symbol_by_index(symbol_idx)?;
                        (symbol.address() as i64 + reloc.addend()) as u32
                    }
                    // RelocationTarget::Section(section_idx) => {
                    //     let section = obj_file.section_by_index(section_idx)?;
                    //     (section.address() as i64 + reloc.addend()) as u32
                    // }
                    // RelocationTarget::Absolute => reloc.addend() as u32,
                    _ => bail!("Invalid .debug relocation target"),
                };
                data[addr as usize..addr as usize + 4].copy_from_slice(&target.to_be_bytes());
            }
            RelocationKind::Elf(elf::R_PPC_NONE) => {}
            _ => bail!("Unhandled .debug relocation type {:?}", reloc.kind()),
        }
    }

    let mut reader = Cursor::new(&*data);
    let tags = read_debug_section(&mut reader)?;

    // let mut w = BufWriter::new(File::create("dwarfdump2.txt")?);
    // for (&addr, tag) in &tags {
    //     writeln!(w, "{}: {:?}", addr, tag)?;
    // }
    // w.flush()?;

    let mut units = Vec::<String>::new();
    if let Some((_, mut tag)) = tags.first_key_value() {
        loop {
            match tag.kind {
                TagKind::CompileUnit => {
                    let unit = tag
                        .string_attribute(AttributeKind::Name)
                        .ok_or_else(|| anyhow!("CompileUnit without name {:?}", tag))?;
                    if units.contains(unit) {
                        log::warn!("Duplicate unit '{}'", unit);
                    } else {
                        units.push(unit.clone());
                    }

                    let children = tag.children(&tags);
                    let mut typedefs = BTreeMap::<u32, Vec<u32>>::new();
                    for child in children {
                        match child.kind {
                            TagKind::GlobalSubroutine | TagKind::Subroutine => {
                                let _is_prototyped =
                                    child.string_attribute(AttributeKind::Prototyped).is_some();
                                if let (Some(_hi), Some(_lo)) = (
                                    child.address_attribute(AttributeKind::HighPc),
                                    child.address_attribute(AttributeKind::LowPc),
                                ) {}
                                let name = child
                                    .string_attribute(AttributeKind::Name)
                                    .ok_or_else(|| anyhow!("Subroutine without name"))?;
                                let udt = ud_type(&tags, child)?;
                                let ts = ud_type_string(&tags, &typedefs, &udt)?;
                                // log::info!("{} {}{};", ts.prefix, name, ts.suffix);
                            }
                            TagKind::Typedef => {
                                let name = child
                                    .string_attribute(AttributeKind::Name)
                                    .ok_or_else(|| anyhow!("Typedef without name"))?;
                                let attr = child
                                    .type_attribute()
                                    .ok_or_else(|| anyhow!("Typedef without type attribute"))?;
                                let t = process_type(attr)?;
                                let ts = type_string(&tags, &typedefs, &t)?;
                                // log::info!("typedef {} {}{};", ts.prefix, name, ts.suffix);

                                // TODO fundamental typedefs?
                                if let Some(ud_type_ref) =
                                    child.reference_attribute(AttributeKind::UserDefType)
                                {
                                    match typedefs.entry(ud_type_ref) {
                                        Entry::Vacant(e) => {
                                            e.insert(vec![child.key]);
                                        }
                                        Entry::Occupied(e) => {
                                            e.into_mut().push(child.key);
                                        }
                                    }
                                }
                            }
                            TagKind::GlobalVariable | TagKind::LocalVariable => {
                                let name = child
                                    .string_attribute(AttributeKind::Name)
                                    .ok_or_else(|| anyhow!("Variable without name"))?;
                                let address = if let Some(location) =
                                    child.block_attribute(AttributeKind::Location)
                                {
                                    Some(process_address(location)?)
                                } else {
                                    None
                                };
                                if let Some(type_attr) = child.type_attribute() {
                                    let var_type = process_type(type_attr)?;
                                    // log::info!("{:?}", var_type);
                                    if let TypeKind::UserDefined(key) = var_type.kind {
                                        let ud_tag = tags
                                            .get(&key)
                                            .ok_or_else(|| anyhow!("Invalid UD type ref"))?;
                                        let ud_type = ud_type(&tags, ud_tag)?;
                                        // log::info!("{:?}", ud_type);
                                    }
                                    let ts = type_string(&tags, &typedefs, &var_type)?;
                                    let st = if child.kind == TagKind::LocalVariable {
                                        "static "
                                    } else {
                                        ""
                                    };
                                    let address_str = match address {
                                        Some(addr) => format!(" : {:#010X}", addr),
                                        None => String::new(),
                                    };
                                    let size = var_type.size(&tags)?;
                                    log::info!(
                                        "{}{} {}{}{}; // size: {:#X}",
                                        st,
                                        ts.prefix,
                                        name,
                                        ts.suffix,
                                        address_str,
                                        size,
                                    );
                                }
                            }
                            TagKind::StructureType
                            | TagKind::ArrayType
                            | TagKind::EnumerationType
                            | TagKind::UnionType
                            | TagKind::ClassType
                            | TagKind::SubroutineType => {
                                let udt = ud_type(&tags, child)?;
                                if child.string_attribute(AttributeKind::Name).is_some() {
                                    // log::info!("{}", ud_type_def(&tags, &typedefs, &udt)?);
                                }
                            }
                            _ => {
                                log::warn!("Unhandled CompileUnit child {:?}", child.kind);
                            }
                        }
                    }
                    // println!("Children: {:?}", children.iter().map(|c| c.kind).collect::<Vec<TagKind>>());
                }
                _ => {
                    log::warn!("Expected CompileUnit, got {:?}", tag.kind);
                    break;
                }
            }
            if let Some(next) = tag.next_sibling(&tags) {
                tag = next;
            } else {
                break;
            }
        }
    }
    // log::info!("Link order:");
    // for x in units {
    //     log::info!("{}", x);
    // }
    Ok(())
}