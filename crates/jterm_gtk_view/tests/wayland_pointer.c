/* Move and click only in the private native Wayland test compositor. */
#define _POSIX_C_SOURCE 200809L
#include <linux/input-event-codes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>

#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    (void)version;
    struct zwlr_virtual_pointer_manager_v1 **manager = data;
    if (!*manager && strcmp(interface, zwlr_virtual_pointer_manager_v1_interface.name) == 0)
        *manager = wl_registry_bind(registry, name, &zwlr_virtual_pointer_manager_v1_interface, 1);
}

static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {registry_global, registry_remove};

int main(void) {
    struct wl_display *display = wl_display_connect(NULL);
    if (!display)
        return 1;
    struct wl_registry *registry = wl_display_get_registry(display);
    struct zwlr_virtual_pointer_manager_v1 *manager = NULL;
    wl_registry_add_listener(registry, &registry_listener, &manager);
    if (wl_display_roundtrip(display) < 0 || !manager)
        return 1;
    struct zwlr_virtual_pointer_v1 *pointer = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(manager, NULL);
    if (wl_display_roundtrip(display) < 0)
        return 1;
    puts("ready");
    fflush(stdout);
    char line[160];
    while (fgets(line, sizeof(line), stdin)) {
        unsigned x, y, width, height;
        char action[6];
        char extra;
        if (sscanf(line, "%5s %u %u %u %u %c", action, &x, &y, &width, &height, &extra) != 5 ||
            (strcmp(action, "click") != 0 && strcmp(action, "move") != 0) ||
            x >= width || y >= height)
            return 2;
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) < 0)
            return 1;
        uint32_t time_ms = (uint32_t)((uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
        zwlr_virtual_pointer_v1_motion_absolute(pointer, time_ms, x, y, width, height);
        if (strcmp(action, "click") == 0) {
            zwlr_virtual_pointer_v1_button(pointer, time_ms, BTN_LEFT, WL_POINTER_BUTTON_STATE_PRESSED);
            zwlr_virtual_pointer_v1_frame(pointer);
            zwlr_virtual_pointer_v1_button(pointer, time_ms, BTN_LEFT, WL_POINTER_BUTTON_STATE_RELEASED);
        }
        zwlr_virtual_pointer_v1_frame(pointer);
        if (wl_display_roundtrip(display) < 0)
            return 1;
        puts("done");
        fflush(stdout);
    }
    zwlr_virtual_pointer_v1_destroy(pointer);
    zwlr_virtual_pointer_manager_v1_destroy(manager);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return 0;
}
