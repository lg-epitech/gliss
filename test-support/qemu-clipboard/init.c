// PID 1 of the opt-in clipboard smoke: a small SPICE agent on QEMU's virtio port,
// standing where spice-vdagent normally sits, so tests see the guest's exact bytes.
// Serial input: "copy <hex>\n" takes the guest clipboard. Serial output:
// "GLISS_PASTE <hex>\n" for text offered by the host, "GLISS_CLIP_READY" once.
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <termios.h>
#include <unistd.h>

enum { MESSAGE_CLIPBOARD = 4, MESSAGE_CAPS = 6, MESSAGE_GRAB = 7, MESSAGE_REQUEST = 8 };
enum { TEXT = 1, CAP_BY_DEMAND = 1 << 5, MAX_CHUNK = 2048, MAX_TEXT = 4 << 20 };
struct chunk { uint32_t port, size; };
struct __attribute__((packed)) message { uint32_t protocol, type; uint64_t opaque; uint32_t size; };

static int port;
static uint8_t incoming[MAX_TEXT], line[2 * MAX_TEXT], copied[MAX_TEXT];
static size_t incoming_len, line_len, copied_len;

static void write_all(int fd, const void *data, size_t size) {
  for (const uint8_t *next = data; size;) {
    ssize_t written = write(fd, next, size);
    if (written <= 0) _exit(1);
    next += written;
    size -= (size_t)written;
  }
}

static void read_all(int fd, void *data, size_t size) {
  for (uint8_t *next = data; size;) {
    ssize_t count = read(fd, next, size);
    if (count <= 0) _exit(1);
    next += count;
    size -= (size_t)count;
  }
}

static void send_message(uint32_t type, uint32_t word, const void *data, uint32_t size) {
  static uint8_t buffer[sizeof(struct message) + 4 + MAX_TEXT];
  struct message header = {1, type, 0, 4 + size};
  memcpy(buffer, &header, sizeof(header));
  memcpy(buffer + sizeof(header), &word, 4);
  if (size) memcpy(buffer + sizeof(header) + 4, data, size);
  size_t total = sizeof(header) + 4 + size;
  for (size_t sent = 0; sent < total;) {
    uint32_t part = total - sent > MAX_CHUNK ? MAX_CHUNK : (uint32_t)(total - sent);
    struct chunk prefix = {1, part};
    write_all(port, &prefix, sizeof(prefix));
    write_all(port, buffer + sent, part);
    sent += part;
  }
}

static void print_paste(const uint8_t *data, size_t size) {
  static char text[2 * MAX_TEXT + 32];
  size_t used = (size_t)sprintf(text, "GLISS_PASTE ");
  for (size_t index = 0; index < size; index++) used += (size_t)sprintf(text + used, "%02x", data[index]);
  text[used++] = '\n';
  write_all(1, text, used);
}

static void handle_message(const struct message *header, const uint8_t *data) {
  static int ready;
  uint32_t word = 0;
  if (header->size >= 4) memcpy(&word, data, 4);
  if (header->type == MESSAGE_CAPS && !ready) {
    ready = 1;
    write_all(1, "GLISS_CLIP_READY\n", 17);
  } else if (header->type == MESSAGE_GRAB && word == TEXT) {
    send_message(MESSAGE_REQUEST, TEXT, NULL, 0);
  } else if (header->type == MESSAGE_REQUEST && word == TEXT) {
    send_message(MESSAGE_CLIPBOARD, TEXT, copied, (uint32_t)copied_len);
  } else if (header->type == MESSAGE_CLIPBOARD && word == TEXT) {
    print_paste(data + 4, header->size - 4);
  }
}

static void read_port(void) {
  struct chunk prefix;
  read_all(port, &prefix, sizeof(prefix));
  if (incoming_len + prefix.size > sizeof(incoming)) _exit(1);
  read_all(port, incoming + incoming_len, prefix.size);
  incoming_len += prefix.size;
  struct message header;
  if (incoming_len < sizeof(header)) return;
  memcpy(&header, incoming, sizeof(header));
  if (incoming_len < sizeof(header) + header.size) return;
  handle_message(&header, incoming + sizeof(header));
  incoming_len = 0;
}

static void read_console(void) {
  uint8_t byte;
  read_all(0, &byte, 1);
  if (byte != '\n') {
    if (line_len < sizeof(line)) line[line_len++] = byte;
    return;
  }
  if (line_len >= 5 && !memcmp(line, "copy ", 5)) {
    copied_len = (line_len - 5) / 2;
    for (size_t index = 0; index < copied_len; index++) {
      char hex[3] = {(char)line[5 + 2 * index], (char)line[6 + 2 * index], 0};
      copied[index] = (uint8_t)strtoul(hex, NULL, 16);
    }
    send_message(MESSAGE_GRAB, TEXT, NULL, 0);
  }
  line_len = 0;
}

static int open_port(void) {
  for (int attempt = 0; attempt < 500; attempt++) {
    DIR *dev = opendir("/dev");
    for (struct dirent *entry; dev && (entry = readdir(dev));) {
      if (strncmp(entry->d_name, "vport", 5)) continue;
      char path[300];
      snprintf(path, sizeof(path), "/dev/%s", entry->d_name);
      closedir(dev);
      return open(path, O_RDWR);
    }
    if (dev) closedir(dev);
    usleep(10000);
  }
  return -1;
}

int main(void) {
  mkdir("/dev", 0755);
  mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
  struct termios raw;
  tcgetattr(0, &raw);
  cfmakeraw(&raw);
  tcsetattr(0, TCSANOW, &raw);
  port = open_port();
  if (port < 0) {
    puts("GLISS_CLIP_FAILED no virtio port; the kernel needs built-in virtio-pci and virtio-console");
    fflush(stdout);
    pause();
  }
  // Announce only by-demand clipboard, so messages carry no selection or serial.
  uint32_t caps[2] = {1, CAP_BY_DEMAND};
  send_message(MESSAGE_CAPS, caps[0], &caps[1], sizeof(caps[1]));
  struct pollfd sources[2] = {{0, POLLIN, 0}, {port, POLLIN, 0}};
  for (;;) {
    if (poll(sources, 2, -1) < 0) _exit(1);
    if (sources[0].revents) read_console();
    if (sources[1].revents) read_port();
  }
}
