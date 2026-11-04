# HSPL standard library: printing words (x86-64 Linux, System V ABI).
#
#   print(word) : word     (returns 0)
#
# Writes the signed decimal representation of the word followed by a newline to
# stdout. Implemented with the write(2) syscall, so it needs no libc symbols
# beyond the startup code the linker already provides.
#
# This file is embedded into the compiler at build time and linked into every
# executable that the driver produces.

        .text
        .globl  print
        .type   print, @function
print:
        pushq   %rbp
        movq    %rsp, %rbp
        subq    $32, %rsp          # scratch buffer, built backwards from %rbp-1

        movq    %rdi, %rax         # value to print
        leaq    -1(%rbp), %rsi     # current write position
        movb    $10, (%rsi)        # '\n'
        movq    $1, %r8            # byte count
        xorl    %r9d, %r9d         # negative flag

        testq   %rax, %rax
        jns     .Ldigits
        negq    %rax
        movl    $1, %r9d

.Ldigits:
        movq    $10, %rcx
.Lloop:
        xorl    %edx, %edx
        divq    %rcx               # %rdx:%rax / 10
        addb    $48, %dl           # remainder -> ASCII digit
        decq    %rsi
        movb    %dl, (%rsi)
        incq    %r8
        testq   %rax, %rax
        jne     .Lloop

        testl   %r9d, %r9d
        je      .Lwrite
        decq    %rsi
        movb    $45, (%rsi)        # '-'
        incq    %r8

.Lwrite:
        movq    $1, %rax           # SYS_write
        movq    $1, %rdi           # stdout
        movq    %r8, %rdx          # count
        syscall

        xorl    %eax, %eax         # return 0
        leave
        ret
        .size   print, .-print

        .section .note.GNU-stack,"",@progbits
