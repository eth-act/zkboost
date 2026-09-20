# Mock ZisK guest. It copies the stdin bytes unchanged to the output region, whose first
# 256 bytes are the public values. ziskos puts the byte length at INPUT_ADDR + 8 and the
# bytes at INPUT_ADDR + 16.
#   riscv64-unknown-elf-as -march=rv64ima -mabi=lp64 -mno-arch-attr -o zisk.o zisk.asm
#   ld.lld -T zisk.ld -o zisk.elf zisk.o

.section .text.init, "ax"
.globl _start
_start:
    li t0, 0x40000008                   # INPUT_ADDR + 8
    ld t1, 0(t0)                        # t1 = byte length
    addi t0, t0, 8
    la t2, _output_data_start           # OUTPUT_ADDR
    beqz t1, exit
    addi t1, t1, 7
    srli t1, t1, 3                      # t1 = 8-byte words
copy:
    ld t3, 0(t0)
    sd t3, 0(t2)
    addi t0, t0, 8
    addi t2, t2, 8
    addi t1, t1, -1
    bnez t1, copy
exit:
    li a7, 93                           # exit ecall of ziskos
    ecall
