#include <stdio.h>
void __toylang_drop_Point(void* ptr) {
    fprintf(stderr, "[toylang] dropping Point at %p\n", ptr);
}
