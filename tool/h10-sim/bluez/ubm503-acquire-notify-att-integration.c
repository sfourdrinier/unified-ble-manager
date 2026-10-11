/* tool/h10-sim/bluez/ubm503-acquire-notify-att-integration.c */
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <signal.h>
#include <unistd.h>

#include <glib.h>

#include "bluetooth/bluetooth.h"
#include "bluetooth/uuid.h"
#include "src/shared/queue.h"
#include "src/shared/att.h"
#include "src/shared/gatt-db.h"
#include "src/shared/gatt-server.h"
#include "src/shared/io.h"
#include "src/shared/util.h"

#define CLI_FEAT_SIZE 1
typedef void DBusMessage;
typedef void DBusConnection;
typedef void DBusMessageIter;
typedef void GDBusClient;
typedef void GDBusProxy;
#define DBG(...) ((void) 0)
#define error(...) ((void) 0)

static void fail(const char *message)
{
	fprintf(stderr, "FAIL: %s\n", message);
	exit(EXIT_FAILURE);
}

static void expect(bool condition, const char *message)
{
	if (!condition)
		fail(message);
}

struct btd_adapter;
struct btd_device { int unused; };
struct btd_gatt_database;
struct notify;
static struct bt_gatt_server *route_server;
static struct device_state *route_state;
static struct ccc_state *route_ccc;
static struct gatt_db *route_database;
static struct btd_gatt_database *route_btd_database;
static struct external_service *route_service;
static struct gatt_app *route_app;
static struct external_chrc *route_chrc;
static struct gatt_db_attribute *route_characteristic;
static struct gatt_db_attribute *route_ccc_attribute;
static struct btd_device route_device;
static struct device_state *find_device_state_by_att(struct btd_gatt_database *database,
		struct bt_att *att);
static struct ccc_state *find_ccc_state(struct device_state *state,
		uint16_t handle);
static struct btd_device *btd_adapter_find_device(struct btd_adapter *adapter,
		const bdaddr_t *bdaddr, uint8_t bdaddr_type);
static struct bt_gatt_server *btd_device_get_gatt_server(struct btd_device *device);
static bool device_is_bonded(struct btd_device *device, uint8_t bdaddr_type);
static void state_set_pending(struct device_state *state, struct notify *notify);
static void clear_ccc_state(void *data, void *user_data);
static void device_state_free(struct device_state *state);
static void patched_sock_io_conf(void *user_data);
static void service_changed_conf(void *user_data);
static unsigned int confirmations;
static unsigned int baseline_confirmations;
static unsigned int guard_allocs;
static unsigned int guard_frees;
static unsigned int client_allocs;
static unsigned int client_frees;
static size_t guard_size;
static void *known_guards[16];
static void *known_clients[16];

static void *tracked_new0(size_t size, size_t count)
{
	void *value = calloc(count, size);

	expect(value != NULL, "tracked allocation");
	if (guard_size && size == guard_size) {
		for (size_t index = 0; index < 16; index++) {
			if (!known_guards[index]) {
				known_guards[index] = value;
				guard_allocs++;
				return value;
			}
		}
		fail("guard registry full");
	}
	return value;
}

static void tracked_free(void *value)
{
	for (size_t index = 0; index < 16; index++) {
		if (known_guards[index] == value) {
			known_guards[index] = NULL;
			guard_frees++;
			break;
		}
		if (known_clients[index] == value) {
			known_clients[index] = NULL;
			client_frees++;
			break;
		}
	}
	free(value);
}

/* Exact client/guard bodies come from the checked patched gatt-database.c. */
#undef new0
#undef free

#define new0(type, count) tracked_new0(sizeof(type), (count))
#define free(value) tracked_free(value)
#define sock_io_conf patched_sock_io_conf
#include "ubm503-acquire-notify-att-integration-extracted.inc"
#undef sock_io_conf
#undef new0
#undef free

static void initialize_tracking(void)
{
	guard_size = sizeof(struct indication_guard);
}

static void patched_sock_io_conf(void *user_data)
{
	confirmations++;
	patched_sock_io_conf_impl(user_data);
}

static struct device_state *find_device_state_by_att(struct btd_gatt_database *database,
		struct bt_att *att)
{
	(void) database;
	(void) att;
	return route_state;
}

static struct ccc_state *find_ccc_state(struct device_state *state,
		uint16_t handle)
{
	(void) state;
	(void) handle;
	return route_ccc;
}

static struct btd_device *btd_adapter_find_device(struct btd_adapter *adapter,
		const bdaddr_t *bdaddr, uint8_t bdaddr_type)
{
	(void) adapter;
	(void) bdaddr;
	(void) bdaddr_type;
	return &route_device;
}

static struct bt_gatt_server *btd_device_get_gatt_server(struct btd_device *device)
{
	(void) device;
	return route_server;
}

static bool device_is_bonded(struct btd_device *device, uint8_t bdaddr_type)
{
	(void) device;
	(void) bdaddr_type;
	return true;
}

static void state_set_pending(struct device_state *state, struct notify *notify)
{
	(void) state;
	(void) notify;
	fail("unexpected pending notification route");
}

static void clear_ccc_state(void *data, void *user_data)
{
	(void) data;
	(void) user_data;
}

static void device_state_free(struct device_state *state)
{
	(void) state;
	fail("unexpected device-state removal route");
}

static void service_changed_conf(void *user_data)
{
	(void) user_data;
}

static void wrapped_baseline_sock_io_conf(void *user_data)
{
	baseline_confirmations++;
	baseline_sock_io_conf(user_data);
}

static void iterate_until(unsigned int expected)
{
	unsigned int steps = 0;

	while (confirmations < expected && steps++ < 200)
		g_main_context_iteration(NULL, TRUE);

	expect(confirmations == expected, "ATT confirmation callback was not delivered");
}

static void pump_main_context(void)
{
	for (unsigned int steps = 0; steps < 200; steps++)
		g_main_context_iteration(NULL, FALSE);
}

static ssize_t receive_pdu(int fd, uint8_t *buffer, size_t size)
{
	ssize_t length;

	length = recv(fd, buffer, size, MSG_DONTWAIT);
	if (length < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
		return 0;
	if (length < 0)
		fail("recv");
	return length;
}

static void setup_att(int sockets[2], struct bt_att **att,
		struct bt_gatt_server **server)
{
	bt_uuid_t uuid;
	struct gatt_db_attribute *service;

	expect(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sockets) == 0,
		"socketpair");
	*att = bt_att_new(sockets[0], false);
	expect(*att != NULL, "bt_att_new");
	route_database = gatt_db_new();
	expect(route_database != NULL, "gatt_db_new");
	gatt_db_ccc_register(route_database, NULL, NULL, NULL, NULL);
	bt_uuid16_create(&uuid, 0x180f);
	service = gatt_db_add_service(route_database, &uuid, true, 5);
	expect(service != NULL, "gatt_db_add_service");
	bt_uuid16_create(&uuid, 0x2a19);
	route_characteristic = gatt_db_service_add_characteristic(service, &uuid,
			0, 0x30, NULL, NULL, NULL);
	expect(route_characteristic != NULL, "gatt_db_add_characteristic");
	route_ccc_attribute = gatt_db_service_add_ccc(route_characteristic, 0x03);
	expect(route_ccc_attribute != NULL, "gatt_db_add_ccc");
	*server = bt_gatt_server_new(route_database, *att, 23, 0);
	expect(*server != NULL, "bt_gatt_server_new");
	route_app = calloc(1, sizeof(*route_app));
	route_btd_database = calloc(1, sizeof(*route_btd_database));
	route_service = calloc(1, sizeof(*route_service));
	route_chrc = calloc(1, sizeof(*route_chrc));
	route_state = calloc(1, sizeof(*route_state));
	route_ccc = calloc(1, sizeof(*route_ccc));
	expect(route_app && route_btd_database && route_service && route_chrc && route_state && route_ccc,
		"route allocation");
	route_btd_database->db = route_database;
	route_app->database = route_btd_database;
	route_service->app = route_app;
	route_chrc->service = route_service;
	route_chrc->attrib = route_characteristic;
	route_chrc->ccc = route_ccc_attribute;
	route_state->att = *att;
	route_state->db = route_btd_database;
	route_ccc->handle = gatt_db_attribute_get_handle(route_ccc_attribute);
	route_ccc->value = 0x0002;
}

static struct client_io *new_client(struct bt_att *att, int fd)
{
	struct client_io *client = calloc(1, sizeof(*client));

	expect(client != NULL, "client allocation");
	client->att = bt_att_ref(att);
	client->io = io_new(fd);
	expect(client->io != NULL, "io_new");
	client->refs = 1;
	client->chrc = route_chrc;
	for (size_t index = 0; index < 16; index++) {
		if (!known_clients[index]) {
			known_clients[index] = client;
			client_allocs++;
			return client;
		}
	}
	fail("client registry full");
	return client;
}

static void discard_att(struct bt_att *att, struct bt_gatt_server *server,
		int sockets[2])
{
	bt_gatt_server_unref(server);
	bt_att_unref(att);
	close(sockets[1]);
	gatt_db_unref(route_database);
	free(route_ccc);
	free(route_state);
	free(route_chrc);
	free(route_service);
	free(route_app);
	free(route_btd_database);
	route_database = NULL;
	route_ccc = NULL;
	route_state = NULL;
	route_chrc = NULL;
	route_service = NULL;
	route_app = NULL;
	route_btd_database = NULL;
}

static void expect_production_cleanup(const char *case_name)
{
	char message[128];

	snprintf(message, sizeof(message), "%s client allocation cleanup", case_name);
	expect(client_allocs == client_frees, message);
	snprintf(message, sizeof(message), "%s guard allocation cleanup", case_name);
	expect(guard_allocs == guard_frees, message);
	for (size_t index = 0; index < 16; index++) {
		expect(!known_clients[index], "retained production client");
		expect(!known_guards[index], "retained production guard");
	}
}

static void run_routed_confirmation(bool retire_before_confirmation)
{
	int sockets[2];
	struct bt_att *att;
	struct bt_gatt_server *server;
	struct client_io *client;
	uint8_t value = retire_before_confirmation ? 0x42 : 0x24;
	uint8_t pdu[32];
	uint8_t confirmation = BT_ATT_OP_HANDLE_CONF;
	ssize_t length;

	setup_att(sockets, &att, &server);
	client = new_client(att, sockets[0]);
	route_server = server;
	route_state->db = route_btd_database;
	expect(send(sockets[1], &value, sizeof(value), 0) == 1,
		"inject acquired IO value");
	expect(sock_io_read(client->io, client), "sock_io_read route");
	pump_main_context();
	length = recv(sockets[1], pdu, sizeof(pdu), 0);
	expect(length == 4 && pdu[0] == BT_ATT_OP_HANDLE_IND,
		"routed real ATT indication packet");

	if (retire_before_confirmation) {
		/* Retire the client before the real ATT confirmation reaches bt_att. */
		client_io_free(client);
		expect(confirmations == 0, "confirmation ran before injected packet");
	}
	expect(send(sockets[1], &confirmation, sizeof(confirmation), 0) == 1,
		"inject ATT confirmation");
	iterate_until(1);
	for (unsigned int steps = 0; steps < 200; steps++)
		g_main_context_iteration(NULL, FALSE);
	length = receive_pdu(sockets[1], pdu, sizeof(pdu));
	if (retire_before_confirmation)
		expect(length == 0, "late confirmation touched retired IO");
	else
		expect(length == 1 && pdu[0] == 1,
			"routed nominal confirmation writes IO byte");
	if (!retire_before_confirmation)
		client_io_free(client);
	discard_att(att, server, sockets);
	expect_production_cleanup("routed late");
	puts(retire_before_confirmation
			? "actual-att-green: sock_io_read route retired-client confirmation delivered without IO write"
			: "actual-att-green: sock_io_read route nominal confirmation delivered and IO write retained");
}

static void run_failed_send_cleanup(void)
{
	int sockets[2];
	struct bt_att *att;
	struct bt_gatt_server *server;
	struct client_io *client;
	struct indication_guard *guard;

	setup_att(sockets, &att, &server);
	client = new_client(att, sockets[0]);
	guard = indication_guard_new(client);
	expect(guard != NULL, "failed-send indication_guard_new");
	expect(!bt_gatt_server_send_indication(server, 3, NULL, 1,
			patched_sock_io_conf, guard, indication_guard_destroy),
		"failed-send must reject invalid value");
	indication_guard_finish(guard, false);
	client_io_free(client);
	discard_att(att, server, sockets);
	expect_production_cleanup("routed nominal");
	puts("actual-att-green: production failed-send synchronous cleanup released guard and client");
}

static void run_baseline_unsafe(void)
{
	int sockets[2];
	struct bt_att *att;
	struct bt_gatt_server *server;
	struct io *io;
	uint8_t value = 0x11;
	uint8_t pdu[32];
	uint8_t confirmation = BT_ATT_OP_HANDLE_CONF;
	ssize_t length;

	setup_att(sockets, &att, &server);
	io = io_new(sockets[0]);
	expect(io != NULL, "baseline io_new");
	expect(bt_gatt_server_send_indication(server, 3, &value, sizeof(value),
			wrapped_baseline_sock_io_conf, io, NULL), "baseline send indication");
	pump_main_context();
	length = recv(sockets[1], pdu, sizeof(pdu), 0);
	expect(length == 4 && pdu[0] == BT_ATT_OP_HANDLE_IND,
		"baseline ATT indication packet");
	io_destroy(io);
	expect(send(sockets[1], &confirmation, sizeof(confirmation), 0) == 1,
		"baseline inject ATT confirmation");
	fprintf(stderr, "baseline-red: actual ATT confirmation injected after IO destroy\n");
	alarm(3);
	for (unsigned int steps = 0; steps < 200; steps++)
		g_main_context_iteration(NULL, FALSE);
	alarm(0);
	expect(baseline_confirmations == 1, "baseline ATT confirmation callback was not delivered");
	discard_att(att, server, sockets);
	expect_production_cleanup("failed send");
	fail("baseline-red: actual ATT late confirmation used destroyed IO");
}

int main(int argc, char **argv)
{
	initialize_tracking();
	if (argc == 2 && strcmp(argv[1], "--baseline") == 0) {
		baseline_confirmations = 0;
		run_baseline_unsafe();
		return EXIT_SUCCESS;
	}

	confirmations = 0;
	run_routed_confirmation(true);
	confirmations = 0;
	run_routed_confirmation(false);
	run_failed_send_cleanup();
	return EXIT_SUCCESS;
}
