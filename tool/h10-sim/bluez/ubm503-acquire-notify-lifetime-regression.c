/* tool/h10-sim/bluez/ubm503-acquire-notify-lifetime-regression.c */
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
struct io { int identity; bool destroyed; unsigned int write_handler_calls; };
struct iovec { void *iov_base; size_t iov_len; };
struct bt_att { unsigned int unref_calls; };
static unsigned int destroyed_ios, use_after_destroy;
static int last_writer_identity;
static unsigned int freed_clients, freed_guards;
static void *known_clients[16], *known_guards[16];
static void *known_ios[16], *known_atts[16];
static void fail(const char *message) { fprintf(stderr, "FAIL: %s\n", message); exit(1); }
static void *test_new0(size_t size) { void *value = calloc(1, size); if (!value) fail("calloc"); return value; }
#define new0(type, count) ((type *) test_new0(sizeof(type) * (count)))
static void bt_att_unregister_disconnect(struct bt_att *att, unsigned int id) { (void) att; (void) id; }
static void bt_att_unref(struct bt_att *att) { att->unref_calls++; }
static void io_destroy(struct io *io) { if (!io || io->destroyed) fail("IO destroyed more than once"); io->destroyed = true; destroyed_ios++; }
static void io_set_write_handler(struct io *io, void *handler, void *user_data, void *destroy) { (void) handler; (void) user_data; (void) destroy; if (!io || io->destroyed) { use_after_destroy++; return; } io->write_handler_calls++; last_writer_identity = io->identity; }
static int io_send(struct io *io, const struct iovec *iov, int count) { (void) io; (void) iov; (void) count; return 0; }
static bool sock_io_write(struct io *io, void *user_data);
/* Exact function bodies are extracted from the deployed baseline and patched source. */
static void tracked_free(void *value);
#define free(value) tracked_free(value)
#include "ubm503-acquire-notify-lifetime-extracted.inc"
#undef free
static void tracked_free(void *value) { unsigned int index; for (index = 0; index < 16; index++) { if (known_clients[index] == value) { freed_clients++; known_clients[index] = NULL; break; } if (known_guards[index] == value) { freed_guards++; known_guards[index] = NULL; break; } } free(value); }
static void remember(void **known, void *value) { unsigned int index; for (index = 0; index < 16; index++) if (!known[index]) { known[index] = value; return; } fail("allocation registry full"); }
static struct client_io *make_client(int identity) { struct client_io *c = new0(struct client_io, 1); remember(known_clients, c); c->att = new0(struct bt_att, 1); remember(known_atts, c->att); c->disconn_id = 1; c->io = new0(struct io, 1); remember(known_ios, c->io); c->refs = 1; c->io->identity = identity; return c; }
static struct indication_guard *make_guard(struct client_io *client) { struct indication_guard *guard = indication_guard_new(client); if (!guard) fail("guard allocation"); remember(known_guards, guard); return guard; }
static void expect(bool condition, const char *message) { if (!condition) fail(message); }
static void finish_counts(unsigned int clients, unsigned int guards) { unsigned int index; expect(freed_clients == clients, "client free count"); expect(freed_guards == guards, "guard free count"); for (index = 0; index < 16; index++) { expect(!known_clients[index], "retained client"); expect(!known_guards[index], "retained guard"); if (known_ios[index]) { free(known_ios[index]); known_ios[index] = NULL; } if (known_atts[index]) { free(known_atts[index]); known_atts[index] = NULL; } } }
static void reset_counters(void) { destroyed_ios = 0; use_after_destroy = 0; last_writer_identity = 0; freed_clients = 0; freed_guards = 0; memset(known_clients, 0, sizeof(known_clients)); memset(known_guards, 0, sizeof(known_guards)); memset(known_ios, 0, sizeof(known_ios)); memset(known_atts, 0, sizeof(known_atts)); }
static void run_baseline_unsafe(void) { struct io *io = new0(struct io, 1); io_destroy(io); baseline_sock_io_conf(io); puts("baseline-red: late confirmation touched destroyed IO"); expect(use_after_destroy == 0, "baseline retired confirmation assertion"); }
static void run_live(void) { struct client_io *c = make_client(1); struct indication_guard *g = make_guard(c); sock_io_conf(g); expect(last_writer_identity == 1, "live writer"); indication_guard_finish(g, true); indication_guard_destroy(g); client_io_free(c); expect(destroyed_ios == 1, "live free"); finish_counts(1, 1); }
static void run_retired(void) { struct client_io *c = make_client(2); struct indication_guard *g = make_guard(c); client_io_free(c); sock_io_conf(g); expect(!use_after_destroy, "retired write"); indication_guard_finish(g, true); indication_guard_destroy(g); expect(destroyed_ios == 1, "retired free"); finish_counts(1, 1); }
static void run_peers(void) { struct client_io *old = make_client(3), *replacement = make_client(4); struct indication_guard *go = make_guard(old), *gr = make_guard(replacement); client_io_free(old); sock_io_conf(go); expect(last_writer_identity == 0, "old generation writer"); sock_io_conf(gr); expect(last_writer_identity == 4, "replacement writer"); indication_guard_finish(go, true); indication_guard_destroy(go); indication_guard_finish(gr, true); indication_guard_destroy(gr); client_io_free(replacement); expect(!use_after_destroy, "peer UAF"); finish_counts(2, 2); }
static void run_failures(void) { struct client_io *a = make_client(5), *b = make_client(6); struct indication_guard *ga = make_guard(a), *gb = make_guard(b); indication_guard_finish(ga, false); client_io_free(a); indication_guard_destroy(gb); indication_guard_finish(gb, false); client_io_free(b); expect(destroyed_ios == 2, "failure lifetime"); finish_counts(2, 2); }
static void run_cancel(void) { struct client_io *c = make_client(7); struct indication_guard *g = make_guard(c); indication_guard_finish(g, true); client_io_free(c); indication_guard_destroy(g); expect(!use_after_destroy && destroyed_ios == 1, "cancel lifetime"); finish_counts(1, 1); }
int main(int argc, char **argv) { if (argc == 2 && !strcmp(argv[1], "--baseline")) { run_baseline_unsafe(); return 0; } reset_counters(); run_live(); reset_counters(); run_retired(); reset_counters(); run_peers(); reset_counters(); run_failures(); reset_counters(); run_cancel(); expect(!use_after_destroy, "patched UAF"); puts("patched-green: exact client and guard frees passed for live, retirement, replacement, failure and cancellation"); return 0; }
