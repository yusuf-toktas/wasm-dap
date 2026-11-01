#include <stdio.h>
#include <string.h>

struct point {
    int x;
    int y;
};

enum color { RED, GREEN = 5, BLUE };

static int counter = 0;
static const char *greeting = "hello from C";

int distance2(const struct point *a, const struct point *b) {
    int dx = a->x - b->x;
    int dy = a->y - b->y;
    return dx * dx + dy * dy; // BP: c-distance
}

int main(int argc, char **argv) {
    struct point points[3] = {{1, 2}, {4, 6}, {-3, 0}};
    enum color shade = GREEN;
    char label[16];
    strcpy(label, "points");
    int total = 0;
    for (int i = 1; i < 3; i++) {
        total += distance2(&points[0], &points[i]);
        counter++;
    }
    printf("%s: total=%d shade=%d argc=%d %s\n", label, total, (int)shade, argc, greeting); // BP: c-print
    return total > 100 ? 1 : 0;
}
