"""Explicit mock authority; never imports Distribox or opens a database.

The fixture owner supplies literal loopback targets. Its instance generation is
authoritative only for the owned test peer, not proof about a real QEMU socket.
"""

import asyncio
from dataclasses import dataclass, field
import hmac
import ipaddress
import math
import secrets
import time
from uuid import UUID, uuid4

from ..adapter import AccessDenied, Grant, RfbTarget


def _lifetime(value, default=300):
    value = default if value is None else value
    if type(value) not in (int, float) or not math.isfinite(value) or not 0 < value <= 3600:
        raise ValueError("mock lifetime must be finite and in 0..3600 seconds")
    return time.time() + value


@dataclass
class MockVm:
    vm_id: str
    node: str
    host: str
    port: int
    credential: str = field(repr=False)
    capabilities: frozenset[str] = frozenset({"display", "input"})
    credential_id: str = field(default_factory=lambda: str(uuid4()))
    credential_deadline: float = 0
    event_deadline: float = 0
    generation: int = 1
    active: bool = True
    credential_enabled: bool = True
    gate: asyncio.Event = field(default_factory=asyncio.Event, repr=False)
    connect_attempts: int = 0


@dataclass
class MockUser:
    user_id: str
    token: str = field(repr=False)
    policies: frozenset[str] = frozenset({"vms:connect"})
    deadline: float = 0
    enabled: bool = True


class MockBackend:
    nodes = ("master", "node_a")

    def __init__(self, configuration):
        if not isinstance(configuration, dict) or configuration.get("mode") != "owned-mock":
            raise ValueError("only explicit owned-mock fixtures are supported")
        if set(configuration) - {"mode", "vms", "users"}:
            raise ValueError("unknown mock configuration field")
        rows = configuration.get("vms")
        if not isinstance(rows, list) or not 1 <= len(rows) <= 8:
            raise ValueError("require one to eight owned VM fixtures")
        self.epoch = secrets.token_hex(16)
        self.vms = {}
        self.users = {}
        self.unavailable = False
        self.resolutions = 0
        for row in rows:
            allowed = {"vm_id", "node", "host", "port", "credential", "credential_ttl", "event_ttl", "capabilities"}
            if not isinstance(row, dict) or set(row) - allowed:
                raise ValueError("invalid mock VM configuration")
            vm_id = str(UUID(row["vm_id"]))
            host = row.get("host", "127.0.0.1")
            port = row["port"]
            credential = row["credential"]
            node = row.get("node", "master")
            capabilities = row.get("capabilities", ["display", "input"])
            if (vm_id in self.vms or node not in self.nodes or
                    not ipaddress.ip_address(host).is_loopback or
                    type(port) is not int or not 1 <= port <= 65535 or
                    not isinstance(credential, str) or not 1 <= len(credential) <= 1024 or
                    not isinstance(capabilities, list) or not all(type(value) is str for value in capabilities) or
                    not set(capabilities) <= {"display", "input"} or "display" not in capabilities):
                raise ValueError("invalid owned mock target or credential")
            vm = MockVm(vm_id, node, host, port, credential,
                        capabilities=frozenset(capabilities),
                        credential_deadline=_lifetime(row.get("credential_ttl")),
                        event_deadline=_lifetime(row.get("event_ttl"), 600))
            vm.gate.set()
            self.vms[vm_id] = vm
        users = configuration.get("users", [])
        if not isinstance(users, list) or len(users) > 8:
            raise ValueError("invalid mock users")
        for row in users:
            if not isinstance(row, dict) or set(row) - {"id", "token", "policies", "ttl"}:
                raise ValueError("invalid mock user configuration")
            user_id = str(UUID(row["id"]))
            token = row["token"]
            policies = row.get("policies", ["vms:connect"])
            if (user_id in self.users or not isinstance(token, str) or not 1 <= len(token) <= 1024 or
                    not isinstance(policies, list) or not all(isinstance(p, str) for p in policies)):
                raise ValueError("invalid mock user")
            self.users[user_id] = MockUser(user_id, token, frozenset(policies), _lifetime(row.get("ttl")))

    def _available(self):
        if self.unavailable:
            raise AccessDenied("mock authority unavailable")

    @staticmethod
    def _policy(user):
        return bool(user.enabled and user.deadline > time.time() and
                    user.policies & {"vms:connect", "distribox:admin"})

    @staticmethod
    def _credential_active(vm):
        return (vm.credential_enabled and vm.credential_deadline > time.time() and
                vm.event_deadline > time.time())

    def authenticate(self, request):
        self._available()
        if request.credential:
            matches = [vm for vm in self.vms.values()
                       if hmac.compare_digest(vm.credential.encode(), request.credential.encode())
                       and self._credential_active(vm)]
            if len(matches) != 1:
                return None
            vm = matches[0]
            return Grant(vm.vm_id, f"credential:{vm.credential_id}",
                         min(vm.credential_deadline, vm.event_deadline), vm.capabilities)
        users = [user for user in self.users.values()
                 if hmac.compare_digest(user.token.encode(), (request.bearer or "").encode())
                 and self._policy(user)]
        if len(users) != 1 or request.vm_id not in self.vms:
            return None
        user = users[0]
        # Same global policy scope as today's Distribox; no invented VM owner.
        return Grant(request.vm_id, f"user:{user.user_id}", user.deadline, self.vms[request.vm_id].capabilities)

    def route_node(self, vm_id):
        self._available()
        vm = self.vms.get(vm_id)
        if vm is None or vm.node not in self.nodes:
            raise AccessDenied("mock VM node unavailable")
        return vm.node

    def reauthorize(self, grant, node):
        self._available()
        vm = self.vms.get(grant.vm_id)
        if vm is None or vm.node != node or grant.auth_expires_at <= time.time():
            return False
        kind, _, subject = grant.subject.partition(":")
        if kind == "user":
            user = self.users.get(subject)
            return user is not None and self._policy(user)
        return (kind == "credential" and vm.credential_id == subject and
                self._credential_active(vm))

    def resolve(self, vm_id, node):
        self._available()
        self.resolutions += 1
        vm = self.vms.get(vm_id)
        if vm is None or not vm.active or vm.node != node:
            raise AccessDenied("owned mock instance unavailable")
        return RfbTarget(vm_id, f"owned-mock:{self.epoch}:{vm.generation}", vm.port, vm.host)

    async def before_connect(self, vm_id):
        vm = self.vms[vm_id]
        vm.connect_attempts += 1
        await vm.gate.wait()

    def mutate(self, body):
        """Test-only state mutation, behind a separate opt-in control secret."""
        if not isinstance(body, dict) or set(body) - {"action", "vm_id", "node", "user_id"}:
            raise ValueError("invalid test mutation")
        action = body.get("action")
        if action == "unavailable":
            self.unavailable = True
        elif action == "available":
            self.unavailable = False
        elif action == "revoke_user":
            self.users[body["user_id"]].policies = frozenset()
        else:
            vm = self.vms[body["vm_id"]]
            if action == "revoke_credential":
                vm.credential_enabled = False
            elif action == "expire_event":
                vm.event_deadline = time.time() - 1
            elif action == "replace_instance":
                vm.generation += 1
            elif action == "stop":
                vm.active = False
                vm.generation += 1
            elif action == "move":
                if body.get("node") not in self.nodes:
                    raise ValueError("invalid node")
                vm.node = body["node"]
                vm.generation += 1
            elif action == "pause_connect":
                vm.gate.clear()
            elif action == "resume_connect":
                vm.gate.set()
            else:
                raise ValueError("unknown test mutation")

    def state(self):
        return {"mode": "owned-mock", "resolutions": self.resolutions,
                "unavailable": self.unavailable,
                "vms": {vm.vm_id: {"generation": vm.generation, "node": vm.node,
                                    "connect_attempts": vm.connect_attempts}
                        for vm in self.vms.values()}}
