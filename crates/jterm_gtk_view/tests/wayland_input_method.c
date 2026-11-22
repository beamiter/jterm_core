/* Disposable input method for the private native Wayland test compositor. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <wayland-client.h>

#include "input-method-unstable-v2-client-protocol.h"

struct input_method {
    struct wl_seat *seat;
    struct zwp_input_method_manager_v2 *manager;
    uint32_t serial;
    bool pending_active;
    bool active;
    bool unavailable;
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    struct input_method *state = data;
    (void)version;
    if (!state->seat && strcmp(interface, wl_seat_interface.name) == 0)
        state->seat = wl_registry_bind(registry, name, &wl_seat_interface, 1);
    if (!state->manager && strcmp(interface, zwp_input_method_manager_v2_interface.name) == 0)
        state->manager = wl_registry_bind(registry, name, &zwp_input_method_manager_v2_interface, 1);
}

static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static void activate(void *data, struct zwp_input_method_v2 *method) {
    (void)method;
    ((struct input_method *)data)->pending_active = true;
}

static void deactivate(void *data, struct zwp_input_method_v2 *method) {
    (void)method;
    ((struct input_method *)data)->pending_active = false;
}

static void surrounding(void *data, struct zwp_input_method_v2 *method,
                        const char *text, uint32_t cursor, uint32_t anchor) {
    (void)data;
    (void)method;
    (void)text;
    (void)cursor;
    (void)anchor;
}

static void cause(void *data, struct zwp_input_method_v2 *method, uint32_t value) {
    (void)data;
    (void)method;
    (void)value;
}

static void content_type(void *data, struct zwp_input_method_v2 *method,
                         uint32_t hint, uint32_t purpose) {
    (void)data;
    (void)method;
    (void)hint;
    (void)purpose;
}

static void done(void *data, struct zwp_input_method_v2 *method) {
    (void)method;
    struct input_method *state = data;
    state->active = state->pending_active;
    state->serial++;
}

static void unavailable(void *data, struct zwp_input_method_v2 *method) {
    (void)method;
    ((struct input_method *)data)->unavailable = true;
}

static const struct wl_registry_listener registry_listener = {registry_global, registry_remove};
static const struct zwp_input_method_v2_listener method_listener = {
    activate, deactivate, surrounding, cause, content_type, done, unavailable,
};

static bool publish(int directory, const char *name) {
    int fd = openat(directory, name, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0)
        return false;
    return close(fd) == 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "Expected the private input-method control directory\n");
        return 2;
    }
    int control = open(argv[1], O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    struct wl_display *display = wl_display_connect(NULL);
    if (control < 0 || !display)
        return 1;
    struct input_method state = {0};
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &state);
    if (wl_display_roundtrip(display) < 0 || !state.manager || !state.seat)
        return 1;
    struct zwp_input_method_v2 *method = zwp_input_method_manager_v2_get_input_method(state.manager, state.seat);
    zwp_input_method_v2_add_listener(method, &method_listener, &state);
    if (wl_display_roundtrip(display) < 0 || state.unavailable || !publish(control, "connected"))
        return 1;

    struct pollfd events = {.fd = wl_display_get_fd(display), .events = POLLIN};
    bool offered = false;
    int status = 1;
    while (!state.unavailable) {
        if (state.active && !offered && faccessat(control, "start", F_OK, 0) == 0) {
            zwp_input_method_v2_set_preedit_string(method, "ni", 2, 2);
            zwp_input_method_v2_commit(method, state.serial);
            if (wl_display_flush(display) < 0 || !publish(control, "preedit"))
                break;
            offered = true;
        }
        if (state.active && offered && faccessat(control, "commit", F_OK, 0) == 0) {
            zwp_input_method_v2_set_preedit_string(method, "", 0, 0);
            zwp_input_method_v2_commit_string(method, "你");
            zwp_input_method_v2_commit(method, state.serial);
            if (wl_display_roundtrip(display) >= 0 && publish(control, "committed"))
                status = 0;
            break;
        }
        int ready = poll(&events, 1, 25);
        if (ready < 0 && errno != EINTR)
            break;
        if (ready > 0 && (events.revents & (POLLERR | POLLHUP | POLLNVAL)))
            break;
        if (ready > 0 && wl_display_dispatch(display) < 0)
            break;
    }
    zwp_input_method_v2_destroy(method);
    zwp_input_method_manager_v2_destroy(state.manager);
    wl_seat_destroy(state.seat);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    close(control);
    return status;
}
