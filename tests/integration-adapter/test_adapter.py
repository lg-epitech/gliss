import concurrent.futures
from dataclasses import replace
import unittest

from integration.distribox.adapter import (
    AccessDenied, DistriboxAdapter, Grant, NodeBroker, RfbTarget,
)


VM = "00000000-0000-4000-8000-000000000001"
OTHER_VM = "00000000-0000-4000-8000-000000000002"
ORIGIN = "http://127.0.0.1:8080"
SECRET = "test-node-control-secret-32-bytes-minimum"


class AdapterTests(unittest.TestCase):
    def setUp(self):
        self.now = 1000
        self.authorized = True
        self.targets = {VM: RfbTarget(VM, "instance-1", 5901)}
        self.grant = Grant(VM, "credential:test", 2000)
        self.auth_requests = []
        self.route_requests = []
        self.node = self.make_node("master")

        def authenticate(request):
            self.auth_requests.append(request)
            if request.credential == "vm-secret" or request.bearer == "valid-jwt":
                return self.grant if self.authorized else None
            return None

        def route(vm_id):
            self.route_requests.append(vm_id)
            return "master"

        self.adapter = DistriboxAdapter(
            allowed_origin=ORIGIN, authenticate=authenticate, route_node=route,
            nodes={"master": (self.node, SECRET)})

    def make_node(self, name, **kwargs):
        return NodeBroker(
            exchange_path=f"/gliss/nodes/{name}/session", allowed_origin=ORIGIN,
            control_secret=SECRET, resolve_target=lambda vm: self.targets[vm],
            reauthorize=lambda grant: self.authorized, clock=lambda: self.now,
            **kwargs)

    def offer(self):
        return self.adapter.create_session({"credential": "vm-secret"}, origin=ORIGIN)

    def test_credential_scope_and_no_target_or_secrets_in_response(self):
        offer = self.offer()
        public = offer.public_dict()
        self.assertEqual(set(public), {"exchange_path", "ticket", "expires_at"})
        self.assertEqual(public["expires_at"], 1030)
        for sensitive in ("vm-secret", SECRET, "5901", "instance-1"):
            self.assertNotIn(sensitive, repr(public))
        self.assertNotIn("vm-secret", repr(self.auth_requests))
        self.assertEqual(self.route_requests, [VM])
        session = self.node.redeem(offer.ticket, origin=ORIGIN)
        self.assertEqual(session.target, self.targets[VM])
        self.assertEqual(session.expires_at, 1300)
        self.assertEqual(session.grant.capabilities, {"display", "input"})

    def test_valid_jwt_is_scoped_to_requested_vm(self):
        offer = self.adapter.create_session({"vm_id": VM}, bearer="valid-jwt", origin=ORIGIN)
        self.assertEqual(self.node.redeem(offer.ticket, origin=ORIGIN).target.vm_id, VM)
        with self.assertRaises(AccessDenied):
            self.adapter.create_session({"vm_id": OTHER_VM}, bearer="valid-jwt", origin=ORIGIN)

    def test_unauthorized_requests_never_resolve_nodes(self):
        for body, bearer in (({"credential": "wrong"}, None),
                             ({"vm_id": VM}, "wrong-jwt")):
            with self.assertRaises(AccessDenied):
                self.adapter.create_session(body, bearer=bearer, origin=ORIGIN)
        self.assertEqual(self.route_requests, [])

    def test_browser_cannot_supply_target_node_or_capabilities(self):
        for field, value in (("host", "127.0.0.1"), ("port", 5901),
                             ("node_id", "master"), ("capabilities", ["resize"]),
                             ("vm_id", VM), ("token", "valid-jwt")):
            with self.assertRaises(AccessDenied):
                self.adapter.create_session({"credential": "vm-secret", field: value}, origin=ORIGIN)
        with self.assertRaises(AccessDenied):
            self.adapter.create_session({"credential": "vm-secret"}, bearer="valid-jwt", origin=ORIGIN)
        self.assertEqual(self.auth_requests, [])

    def test_exact_origin_enforced_without_consuming_ticket(self):
        offer = self.offer()
        for origin in (None, "", "http://evil.invalid", ORIGIN + "/"):
            with self.assertRaises(AccessDenied):
                self.adapter.create_session({"credential": "vm-secret"}, origin=origin)
            with self.assertRaises(AccessDenied):
                self.node.redeem(offer.ticket, origin=origin)
        self.node.redeem(offer.ticket, origin=ORIGIN)

    def test_ticket_replay_and_cross_node_rejected(self):
        offer = self.offer()
        with self.assertRaises(AccessDenied):
            self.make_node("slave").redeem(offer.ticket, origin=ORIGIN)
        self.node.redeem(offer.ticket, origin=ORIGIN)
        with self.assertRaises(AccessDenied):
            self.node.redeem(offer.ticket, origin=ORIGIN)

    def test_concurrent_ticket_consumption_only_succeeds_once(self):
        offer = self.offer()

        def redeem(_):
            try:
                self.node.redeem(offer.ticket, origin=ORIGIN)
                return True
            except AccessDenied:
                return False

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            self.assertEqual(sum(executor.map(redeem, range(4))), 1)

    def test_expiration_and_authorization_deadline(self):
        self.grant = replace(self.grant, auth_expires_at=1010)
        offer = self.offer()
        self.assertEqual(offer.expires_at, 1010)
        self.now = 1010
        with self.assertRaises(AccessDenied):
            self.node.redeem(offer.ticket, origin=ORIGIN)

    def test_active_session_stops_after_deadline_or_revocation(self):
        session = self.node.redeem(self.offer().ticket, origin=ORIGIN)
        self.authorized = False
        self.assertFalse(self.node.still_authorized(session))
        self.authorized = True
        self.now = 1300
        self.assertFalse(self.node.still_authorized(session))

    def test_revocation_and_restart_rejected_before_connection(self):
        offer = self.offer()
        self.authorized = False
        with self.assertRaises(AccessDenied):
            self.node.redeem(offer.ticket, origin=ORIGIN)
        self.authorized = True
        offer = self.offer()
        self.targets[VM] = replace(self.targets[VM], instance="instance-2")
        with self.assertRaises(AccessDenied):
            self.node.redeem(offer.ticket, origin=ORIGIN)

    def test_loopback_only_and_canonical_target_vm(self):
        for host in ("example.com", "localhost", "192.0.2.1", "0.0.0.0"):
            self.targets[VM] = RfbTarget(VM, "instance", 5901, host)
            with self.assertRaises(AccessDenied):
                self.offer()
        self.targets[VM] = RfbTarget(OTHER_VM, "instance", 5901)
        with self.assertRaises(AccessDenied):
            self.offer()

    def test_unregistered_node_and_wrong_internal_secret_fail_closed(self):
        self.adapter._nodes = {}
        with self.assertRaises(AccessDenied):
            self.offer()
        with self.assertRaises(AccessDenied):
            self.node.enroll(self.grant, control_secret="wrong", origin=ORIGIN)

    def test_bounded_store_prunes_expired_offers(self):
        node = self.make_node("small", max_pending=1)
        node.enroll(self.grant, control_secret=SECRET, origin=ORIGIN)
        with self.assertRaises(AccessDenied):
            node.enroll(self.grant, control_secret=SECRET, origin=ORIGIN)
        self.now = 1030
        node.enroll(self.grant, control_secret=SECRET, origin=ORIGIN)


if __name__ == "__main__":
    unittest.main()
