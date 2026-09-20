# Mock SP1 guest. It commits the stdin bytes unchanged as the public values. A syscall is an
# ecall with the code of sp1 crates/core/executor/src/syscall_code.rs in t0 and the arguments
# in a0 to a2. The verifier needs the SHA-256 digest of the public values committed as 8 words.
#   riscv64-unknown-elf-as -march=rv64im -mabi=lp64 -mno-arch-attr -o sp1.o sp1.asm
#   ld.lld -T sp1.ld -o sp1.elf sp1.o

.text
.globl _start
_start:
    li t0, 0xf0                         # HINT_LEN
    ecall
    mv s0, t0                           # s0 = byte length
    la s1, buffer
    li t0, 0xf1                         # HINT_READ into the buffer, plus one zero word
    mv a0, s1
    mv a1, s0
    ecall
    li t0, 2                            # WRITE to the public values, fd 13
    li a0, 13
    mv a1, s1
    mv a2, s0
    ecall

    # SHA-256 padding, 0x80 after the message and the big-endian bit length last
    add t1, s1, s0
    li t2, 0x80
    sb t2, 0(t1)
    addi s2, s0, 8
    srli s2, s2, 6
    addi s2, s2, 1
    slli s2, s2, 6                      # s2 = padded length
    slli t1, s0, 3
    add t2, s1, s2
    addi t2, t2, -8
    addi t3, t2, 8
bit_length:
    addi t3, t3, -1
    sb t1, 0(t3)
    srli t1, t1, 8
    bne t3, t2, bit_length

    # one 64-byte block per iteration, s4 = block, s5 = end
    mv s4, s1
    add s5, s1, s2
    la s3, state
    la s6, schedule
block:
    li t1, 0
    li t3, 64
fill:                                   # schedule[i] = big-endian u32 at block[4 * i]
    add t4, s4, t1
    lbu t5, 0(t4)
    lbu t6, 1(t4)
    slli t5, t5, 8
    or t5, t5, t6
    lbu t6, 2(t4)
    slli t5, t5, 8
    or t5, t5, t6
    lbu t6, 3(t4)
    slli t5, t5, 8
    or t5, t5, t6
    slli t4, t1, 1
    add t4, s6, t4
    sd t5, 0(t4)
    addi t1, t1, 4
    bne t1, t3, fill
    li t0, 0x00300105                   # SHA_EXTEND schedule
    mv a0, s6
    li a1, 0
    ecall
    li t0, 0x00010106                   # SHA_COMPRESS schedule into state
    mv a0, s6
    mv a1, s3
    ecall
    addi s4, s4, 64
    bne s4, s5, block

    li s7, 0
commit:                                 # COMMIT word i = bswap32(state[i])
    slli t1, s7, 3
    add t1, s3, t1
    lwu t2, 0(t1)
    srli a1, t2, 24
    srli t3, t2, 16
    andi t3, t3, 0xff
    slli t3, t3, 8
    or a1, a1, t3
    slli t3, t2, 8
    li t4, 0xff0000
    and t3, t3, t4
    or a1, a1, t3
    andi t3, t2, 0xff
    slli t3, t3, 24
    or a1, a1, t3
    li t0, 0x10
    mv a0, s7
    ecall
    addi s7, s7, 1
    li t1, 8
    bne s7, t1, commit

    li s7, 0
deferred:                               # COMMIT_DEFERRED_PROOFS word i = 0
    li t0, 0x1a
    mv a0, s7
    li a1, 0
    ecall
    addi s7, s7, 1
    li t1, 8
    bne s7, t1, deferred

    li t0, 0                            # HALT 0
    li a0, 0
    ecall

.data
.balign 8
state:                                  # the SHA-256 initial state in 8-byte slots
    .quad 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a
    .quad 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19

.bss
.balign 8
schedule: .space 512
buffer:                                 # the message, open ended at the image end
