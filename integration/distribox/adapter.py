"""Executable reference contract; no HTTP server and no Distribox imports.

Production wiring must supply backend authentication and per-node local target
resolution. This is separate from Gliss's standalone fixed-target token login.
"""

from dataclasses import dataclass, field
import hashlib
import hmac
import ipaddress
import re
import secrets
from threading import Lock
import time
from typing import Callable, Mapping
from uuid import UUID


class AccessDenied(ValueError):
    pass


@dataclass(frozen=True)
class AccessRequest:
    vm_id: str | None = None
    bearer: str | None = field(default=None, repr=False)
    credential: str | None = field(default=None, repr=False)

    @classmethod
    def parse(cls, body: Mapping[str, object], bearer: str | None):
        if not isinstance(body, dict):
            raise AccessDenied("Invalid request")
        if bearer:
            if set(body) != {"vm_id"} or not isinstance(body["vm_id"], str):
                raise AccessDenied("Use bearer authentication with a VM id")
            try:
                return cls(vm_id=str(UUID(body["vm_id"])), bearer=bearer)
            except ValueError as exc:
                raise AccessDenied("Invalid VM id") from exc
        if (set(body) != {"credential"} or
                not isinstance(body["credential"], str) or
                not 1 <= len(body["credential"]) <= 1024):
            raise AccessDenied("Use a VM access credential")
        return cls(credential=body["credential"])


@dataclass(frozen=True)
class Grant:
    vm_id: str
    subject: str  # e.g. user:<uuid> or credential:<uuid>; never a raw secret.
    auth_expires_at: float
    capabilities: frozenset[str] = frozenset({"display", "input"})


@dataclass(frozen=True)
class RfbTarget:
    vm_id: str
    instance: str  # Local domain UUID + running domain id / lifecycle epoch.
    port: int
    host: str = "127.0.0.1"

    def validate(self, vm_id: str):
        try:
            local = ipaddress.ip_address(self.host).is_loopback
            canonical = str(UUID(self.vm_id))
        except ValueError as exc:
            raise AccessDenied("Invalid node target") from exc
        if (canonical != vm_id or not local or not self.instance or
                type(self.port) is not int or not 1 <= self.port <= 65535):
            raise AccessDenied("Invalid node target")


@dataclass(frozen=True)
class SessionOffer:
    exchange_path: str  # Same-origin node proxy path, no node host / VNC port.
    ticket: str = field(repr=False)
    expires_at: float

    def public_dict(self):
        return {"exchange_path": self.exchange_path,
                "ticket": self.ticket, "expires_at": self.expires_at}


@dataclass(frozen=True)
class StreamSession:
    grant: Grant
    target: RfbTarget
    expires_at: float


@dataclass(frozen=True)
class _Pending:
    grant: Grant
    target: RfbTarget
    ticket_expires_at: float
    session_expires_at: float


class NodeBroker:
    """Per-node opaque, single-use tickets with bounded memory.

    The control secret authenticates trusted enrollment, never a browser call.
    After redeem, HTTP wiring must store StreamSession under an independent
    HttpOnly cookie, enforce its deadline, and call still_authorized periodically.
    """

    def __init__(self, *, exchange_path: str, allowed_origin: str,
                 control_secret: str, resolve_target: Callable[[str], RfbTarget],
                 reauthorize: Callable[[Grant], bool], clock=time.time,
                 max_pending: int = 128):
        if not re.fullmatch(r"/gliss/nodes/[A-Za-z0-9_-]+/session", exchange_path):
            raise ValueError("Require a configured same-origin node proxy path")
        if len(control_secret) < 32 or max_pending < 1:
            raise ValueError("Require a node control secret and a bounded store")
        self.exchange_path = exchange_path
        self.allowed_origin = allowed_origin
        self._control_secret = control_secret
        self._resolve_target = resolve_target
        self._reauthorize = reauthorize
        self._clock = clock
        self._max_pending = max_pending
        self._pending: dict[bytes, _Pending] = {}
        self._lock = Lock()

    def _origin(self, origin):
        if not origin or origin != self.allowed_origin:
            raise AccessDenied("Origin rejected")

    def enroll(self, grant: Grant, *, control_secret: str, origin: str):
        self._origin(origin)
        if not hmac.compare_digest(control_secret.encode(), self._control_secret.encode()):
            raise AccessDenied("Node enrollment rejected")
        now = self._clock()
        try:
            vm_id = str(UUID(grant.vm_id))
        except ValueError as exc:
            raise AccessDenied("Invalid grant") from exc
        if (vm_id != grant.vm_id or not grant.subject or
                not grant.capabilities <= {"display", "input", "clipboard", "resize"} or
                "display" not in grant.capabilities or
                not now < grant.auth_expires_at <= float("inf")):
            raise AccessDenied("Invalid or expired grant")
        target = self._resolve_target(vm_id)
        target.validate(vm_id)
        ticket_deadline = min(now + 30, grant.auth_expires_at)
        session_deadline = min(now + 300, grant.auth_expires_at)
        ticket = secrets.token_urlsafe(32)
        key = hashlib.sha256(ticket.encode()).digest()
        with self._lock:
            self._pending = {key: entry for key, entry in self._pending.items()
                             if entry.ticket_expires_at > now}
            if len(self._pending) >= self._max_pending:
                raise AccessDenied("Node session capacity reached")
            self._pending[key] = _Pending(grant, target, ticket_deadline, session_deadline)
        return SessionOffer(self.exchange_path, ticket, ticket_deadline)

    def redeem(self, ticket: str, *, origin: str):
        self._origin(origin)
        if not isinstance(ticket, str) or not 1 <= len(ticket) <= 128:
            raise AccessDenied("Invalid stream ticket")
        key = hashlib.sha256(ticket.encode()).digest()
        with self._lock:
            entry = self._pending.pop(key, None)
        if entry is None or entry.ticket_expires_at <= self._clock():
            raise AccessDenied("Invalid or expired stream ticket")
        session = StreamSession(entry.grant, entry.target, entry.session_expires_at)
        if not self.still_authorized(session):
            raise AccessDenied("Stream authorization changed")
        return session

    def still_authorized(self, session: StreamSession):
        if session.expires_at <= self._clock():
            return False
        try:
            # Check current backend policy/credential state and the VM instance;
            # autoassigned ports may be recycled after a stop/restart.
            if not self._reauthorize(session.grant):
                return False
            current = self._resolve_target(session.grant.vm_id)
            current.validate(session.grant.vm_id)
            return current == session.target
        except Exception:
            return False


class DistriboxAdapter:
    """Master-side bridge: authorize first, then select a registered node."""

    def __init__(self, *, allowed_origin: str,
                 authenticate: Callable[[AccessRequest], Grant | None],
                 route_node: Callable[[str], str],
                 nodes: Mapping[str, tuple[NodeBroker, str]]):
        self.allowed_origin = allowed_origin
        self._authenticate = authenticate
        self._route_node = route_node
        self._nodes = dict(nodes)

    def create_session(self, body, *, bearer=None, origin):
        if not origin or origin != self.allowed_origin:
            raise AccessDenied("Origin rejected")
        request = AccessRequest.parse(body, bearer)
        grant = self._authenticate(request)
        if grant is None or (request.vm_id and request.vm_id != grant.vm_id):
            raise AccessDenied("VM access rejected")
        node_id = self._route_node(grant.vm_id)
        if node_id not in self._nodes:
            raise AccessDenied("VM node unavailable")
        node, secret = self._nodes[node_id]
        return node.enroll(grant, control_secret=secret, origin=origin)
