"""List a wasm module's imports and exports; with --none, fail on any import.

A compiled playbook must have no imports: it cannot do I/O, read a clock or
draw randomness because there is nothing to call.
"""
import sys


def leb(b, i):
    r = s = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7F) << s
        s += 7
        if x < 0x80:
            return r, i


def name(b, i):
    n, i = leb(b, i)
    return b[i:i + n].decode(), i + n


def scan(path):
    b = open(path, "rb").read()
    assert b[:4] == b"\0asm", f"{path}: not a wasm module"
    i, imports, exports = 8, [], []
    while i < len(b):
        sid = b[i]
        n, i = leb(b, i + 1)
        end = i + n
        if sid == 2:
            c, j = leb(b, i)
            for _ in range(c):
                m, j = name(b, j)
                f, j = name(b, j)
                k = b[j]
                j += 1
                if k == 0:
                    _, j = leb(b, j)
                elif k in (1, 2):
                    if k == 1:
                        j += 1
                    fl, j = leb(b, j)
                    _, j = leb(b, j)
                    if fl & 1:
                        _, j = leb(b, j)
                else:
                    j += 2
                imports.append(f"{m}.{f}")
        elif sid == 7:
            c, j = leb(b, i)
            for _ in range(c):
                f, j = name(b, j)
                j += 1
                _, j = leb(b, j)
                exports.append(f)
        i = end
    return imports, exports


if __name__ == "__main__":
    none = "--none" in sys.argv
    for path in [a for a in sys.argv[1:] if a != "--none"]:
        imports, exports = scan(path)
        print(f"{path}: imports {imports or 'none'}, exports {exports}")
        if none and imports:
            sys.exit(f"FAIL: {path} imports {imports}")
