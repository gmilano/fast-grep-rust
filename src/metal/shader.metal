#include <metal_stdlib>
using namespace metal;

struct LineEntry {
    uint offset;  // byte offset into line_data buffer
    uint length;  // length of this line in bytes
};

/// Each GPU thread checks one candidate line for the needle literal.
/// results[id] = 1 if line contains needle, 0 otherwise.
kernel void literal_search(
    device const uchar* line_data [[ buffer(0) ]],
    device const LineEntry* entries [[ buffer(1) ]],
    device const uchar* needle [[ buffer(2) ]],
    constant uint& needle_len [[ buffer(3) ]],
    device uint* results [[ buffer(4) ]],
    uint id [[ thread_position_in_grid ]]
) {
    uint offset = entries[id].offset;
    uint len = entries[id].length;

    if (len < needle_len) {
        results[id] = 0;
        return;
    }

    for (uint i = 0; i <= len - needle_len; i++) {
        bool match = true;
        for (uint j = 0; j < needle_len && match; j++) {
            if (line_data[offset + i + j] != needle[j]) {
                match = false;
            }
        }
        if (match) {
            results[id] = 1;
            return;
        }
    }
    results[id] = 0;
}
