# Mock OpenVM guest. It reveals the stdin bytes unchanged as the 256-byte public values.
# Custom instructions use opcode 0x0b with funct3 0 terminate, 1 hint, 2 reveal, 3 phantom.
#   riscv64-unknown-elf-as -march=rv64im -mabi=lp64 -mno-arch-attr -o openvm.o openvm.asm
#   ld.lld -T openvm.ld -o openvm.elf openvm.o

.text
.globl _start
_start:
    .insn i 0x0b, 3, x0, x0, 0          # hint_input, the stdin record
    la a0, hint_length
    .insn i 0x0b, 1, a0, x0, 0          # hint_store_u64, the byte length
    ld a1, 0(a0)
    addi a1, a1, 7
    srli a1, a1, 3                      # a1 = 8-byte words
    beqz a1, terminate
    la a0, buffer
    .insn i 0x0b, 1, a0, a1, 1          # hint_buffer_u64, a1 words
    li t0, 0
    slli t1, a1, 3
reveal:
    add t2, a0, t0
    ld a2, 0(t2)
    .insn i 0x0b, 2, t0, a2, 0          # reveal a2 at byte index t0
    addi t0, t0, 8
    bltu t0, t1, reveal
terminate:
    .insn i 0x0b, 0, x0, x0, 0          # terminate 0

.bss
.balign 8
hint_length: .space 8
buffer: .space 256
