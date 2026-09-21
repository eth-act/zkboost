/* The layout of https://github.com/openvm-org/openvm/blob/v2.1.0-preview/crates/toolchain/build/src/lib.rs, which links with -Ttext=0x00200800. */
ENTRY(_start)
SECTIONS {
  . = 0x00200800;
  .text : { *(.text) }
  .bss : { *(.bss) }
}
