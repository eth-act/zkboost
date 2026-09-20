/* The layout of https://github.com/succinctlabs/sp1/blob/v6.4.0/crates/build/src/command/utils.rs, which links with --image-base=0x78000000. */
ENTRY(_start)
SECTIONS {
  . = 0x78000000;
  .text : { *(.text) }
  .data : { *(.data) }
  .bss : { *(.bss) }
}
