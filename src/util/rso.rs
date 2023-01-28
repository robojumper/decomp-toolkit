use std::{io::Read, path::Path};

use anyhow::{anyhow, ensure, Result};
use byteorder::{BigEndian, ReadBytesExt};
use cwdemangle::{demangle, DemangleOptions};

use crate::{
    obj::{
        ObjArchitecture, ObjInfo, ObjKind, ObjSection, ObjSectionKind, ObjSymbol, ObjSymbolFlagSet,
        ObjSymbolFlags, ObjSymbolKind,
    },
    util::file::{map_file, map_reader, read_c_string, read_string},
};

pub fn process_rso<P: AsRef<Path>>(path: P) -> Result<ObjInfo> {
    let mmap = map_file(path)?;
    let mut reader = map_reader(&mmap);

    ensure!(reader.read_u32::<BigEndian>()? == 0, "Expected 'next' to be 0");
    ensure!(reader.read_u32::<BigEndian>()? == 0, "Expected 'prev' to be 0");
    let num_sections = reader.read_u32::<BigEndian>()?;
    let section_info_offset = reader.read_u32::<BigEndian>()?;
    let name_offset = reader.read_u32::<BigEndian>()?;
    let name_size = reader.read_u32::<BigEndian>()?;
    let version = reader.read_u32::<BigEndian>()?;
    ensure!(version == 1, "Unsupported RSO version {}", version);
    let bss_size = reader.read_u32::<BigEndian>()?;
    let prolog_section = reader.read_u8()?;
    let epilog_section = reader.read_u8()?;
    let unresolved_section = reader.read_u8()?;
    ensure!(reader.read_u8()? == 0, "Expected 'bssSection' to be 0");
    let prolog_offset = reader.read_u32::<BigEndian>()?;
    let epilog_offset = reader.read_u32::<BigEndian>()?;
    let unresolved_offset = reader.read_u32::<BigEndian>()?;
    let internal_rel_offset = reader.read_u32::<BigEndian>()?;
    let internal_rel_size = reader.read_u32::<BigEndian>()?;
    let external_rel_offset = reader.read_u32::<BigEndian>()?;
    let external_rel_size = reader.read_u32::<BigEndian>()?;
    let export_table_offset = reader.read_u32::<BigEndian>()?;
    let export_table_size = reader.read_u32::<BigEndian>()?;
    let export_table_name_offset = reader.read_u32::<BigEndian>()?;
    let import_table_offset = reader.read_u32::<BigEndian>()?;
    let import_table_size = reader.read_u32::<BigEndian>()?;
    let import_table_name_offset = reader.read_u32::<BigEndian>()?;

    let mut sections = Vec::with_capacity(num_sections as usize);
    reader.set_position(section_info_offset as u64);
    let mut total_bss_size = 0;
    for idx in 0..num_sections {
        let offset = reader.read_u32::<BigEndian>()?;
        let size = reader.read_u32::<BigEndian>()?;
        log::info!("Section {}: {:#X} {:#X}", idx, offset, size);
        if size == 0 {
            continue;
        }
        let exec = (offset & 1) == 1;
        let offset = offset & !3;

        let data = if offset == 0 {
            vec![]
        } else {
            let position = reader.position();
            reader.set_position(offset as u64);
            let mut data = vec![0u8; size as usize];
            reader.read_exact(&mut data)?;
            reader.set_position(position);
            data
        };

        // println!("Section {} offset {:#X} size {:#X}", idx, offset, size);

        let index = sections.len();
        sections.push(ObjSection {
            name: format!(".section{}", idx),
            kind: if offset == 0 {
                ObjSectionKind::Bss
            } else if exec {
                ObjSectionKind::Code
            } else {
                ObjSectionKind::Data
            },
            address: 0,
            size: size as u64,
            data,
            align: 0,
            index,
            elf_index: idx as usize,
            relocations: vec![],
            original_address: 0,
            file_offset: offset as u64,
            section_known: false,
        });
        if offset == 0 {
            total_bss_size += size;
        }
    }
    ensure!(
        total_bss_size == bss_size,
        "Mismatched BSS size: {:#X} != {:#X}",
        total_bss_size,
        bss_size
    );

    let mut symbols = Vec::new();
    let mut add_symbol = |section_idx: u8, offset: u32, name: &str| -> Result<()> {
        if section_idx > 0 {
            let section = sections
                .iter()
                .find(|section| section.elf_index == section_idx as usize)
                .ok_or_else(|| anyhow!("Failed to locate {name} section {section_idx}"))?;
            log::info!("Adding {name} section {section_idx} offset {offset:#X}");
            symbols.push(ObjSymbol {
                name: name.to_string(),
                demangled_name: None,
                address: offset as u64,
                section: Some(section.index),
                size: 0,
                size_known: false,
                flags: ObjSymbolFlagSet(ObjSymbolFlags::Global.into()),
                kind: ObjSymbolKind::Function,
            });
        }
        Ok(())
    };
    add_symbol(prolog_section, prolog_offset, "_prolog")?;
    add_symbol(epilog_section, epilog_offset, "_epilog")?;
    add_symbol(unresolved_section, unresolved_offset, "_unresolved")?;

    reader.set_position(external_rel_offset as u64);
    while reader.position() < (external_rel_offset + external_rel_size) as u64 {
        let offset = reader.read_u32::<BigEndian>()?;
        let id_and_type = reader.read_u32::<BigEndian>()?;
        let id = (id_and_type & 0xFFFFFF00) >> 8;
        let rel_type = id_and_type & 0xFF;
        let sym_offset = reader.read_u32::<BigEndian>()?;
        log::info!(
            "Reloc offset: {:#X}, id: {}, type: {}, sym offset: {:#X}",
            offset,
            id,
            rel_type,
            sym_offset
        );
    }

    reader.set_position(export_table_offset as u64);
    while reader.position() < (export_table_offset + export_table_size) as u64 {
        let name_off = reader.read_u32::<BigEndian>()?;
        let name = read_c_string(&mut reader, (export_table_name_offset + name_off) as u64)?;
        let sym_off = reader.read_u32::<BigEndian>()?;
        let section_idx = reader.read_u32::<BigEndian>()?;
        let hash_n = reader.read_u32::<BigEndian>()?;
        let calc = symbol_hash(&name);
        let demangled_name = demangle(&name, &DemangleOptions::default());
        let section = sections
            .iter()
            .find(|section| section.elf_index == section_idx as usize)
            .map(|section| section.index);
        log::info!(
            "Export: {}, sym off: {:#X}, section: {}, ELF hash: {:#X}, {:#X}",
            demangled_name.as_deref().unwrap_or(&name),
            sym_off,
            section_idx,
            hash_n,
            calc
        );
        symbols.push(ObjSymbol {
            name,
            demangled_name,
            address: sym_off as u64,
            section,
            size: 0,
            size_known: false,
            flags: Default::default(),
            kind: Default::default(),
        });
    }
    reader.set_position(import_table_offset as u64);
    while reader.position() < (import_table_offset + import_table_size) as u64 {
        let name_off = reader.read_u32::<BigEndian>()?;
        let name = read_c_string(&mut reader, (import_table_name_offset + name_off) as u64)?;
        let sym_off = reader.read_u32::<BigEndian>()?;
        let section_idx = reader.read_u32::<BigEndian>()?;
        log::info!("Import: {}, sym off: {}, section: {}", name, sym_off, section_idx);
    }

    let name = match name_offset {
        0 => String::new(),
        _ => read_string(&mut reader, name_offset as u64, name_size as usize)?,
    };
    Ok(ObjInfo {
        kind: ObjKind::Relocatable,
        architecture: ObjArchitecture::PowerPc,
        name,
        symbols,
        sections,
        entry: 0,
        sda2_base: None,
        sda_base: None,
        stack_address: None,
        stack_end: None,
        db_stack_addr: None,
        arena_lo: None,
        arena_hi: None,
        splits: Default::default(),
        named_sections: Default::default(),
        link_order: vec![],
        known_functions: Default::default(),
        module_id: 0,
        unresolved_relocations: vec![],
    })
}

fn symbol_hash(s: &str) -> u32 {
    s.bytes().fold(0u32, |hash, c| {
        let mut m = (hash << 4) + c as u32;
        let n = m & 0xF0000000;
        if n != 0 {
            m ^= n >> 24;
        }
        m & !n
    })
}