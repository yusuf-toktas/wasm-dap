const std = @import("std");

const Pair = struct {
    a: i32,
    b: i32,
};

fn combine(p: Pair, scale: i32) i32 {
    const sum = p.a + p.b;
    return sum * scale; // BP: zig-combine
}

pub fn main() void {
    const pairs = [_]Pair{ .{ .a = 1, .b = 2 }, .{ .a = 10, .b = 20 } };
    var total: i32 = 0;
    for (pairs) |p| {
        total += combine(p, 3);
    }
    std.debug.print("total={d}\n", .{total});
}
