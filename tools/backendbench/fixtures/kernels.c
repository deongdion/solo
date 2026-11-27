/* Freestanding guest kernels for backend benchmarks. No libc, no relocations. */

unsigned empty(void) { return 0; }

/* FNV-style hash over a word buffer, repeated `rounds` times: ALU plus sequential loads. */
unsigned checksum(const unsigned *buf, unsigned words, unsigned rounds) {
    unsigned h = 0x811c9dc5u;
    for (unsigned r = 0; r < rounds; r++) {
        for (unsigned i = 0; i < words; i++) {
            h = (h ^ buf[i]) * 16777619u;
            h = (h << 7) | (h >> 25);
        }
    }
    return h;
}

/* Byte copy loop: loads and stores. */
unsigned copy(unsigned char *dst, const unsigned char *src, unsigned len, unsigned rounds) {
    unsigned s = 0;
    for (unsigned r = 0; r < rounds; r++) {
        for (unsigned i = 0; i < len; i++) {
            dst[i] = (unsigned char)(src[i] + r);
        }
        s += dst[len - 1];
    }
    return s;
}
