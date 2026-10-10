"""Choose your own login: accounts, the session token, its rotation, and the
authenticator's rules."""

from __future__ import annotations

import asyncio
import json
import re
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "deploy" / "hub"))

from cactup_cyol import session  # noqa: E402
from cactup_cyol.accounts import Accounts, check_password, hash_password  # noqa: E402
from cactup_cyol.rotator import Rotator  # noqa: E402

TEMPLATE = re.compile(r"^[a-z]+ [a-z]+ \d{1,2} [a-z]+ [a-z]+[!?.]$")


# -- the token ----------------------------------------------------------------------

def test_tokens_follow_the_template_and_differ() -> None:
    tokens = {session.generate() for _ in range(50)}
    assert len(tokens) == 50
    for token in tokens:
        assert TEMPLATE.match(token), token
        assert 2 <= int(token.split()[2]) <= 99


def test_word_lists_are_lowercase_unique_and_pluralize_regularly() -> None:
    for name in ("nouns", "verbs", "adjectives"):
        words = session.load_words(name)
        assert len(words) >= 100, name
        assert len(set(words)) == len(words), name
        assert all(re.fullmatch(r"[a-z]+", w) for w in words), name
    assert session.plural("fox") == "foxes"
    assert session.plural("daisy") == "daisies"
    assert session.plural("donkey") == "donkeys"
    assert session.plural("harp") == "harps"


def test_typing_is_forgiving_about_case_spacing_and_punctuation() -> None:
    state = session.State(current="bedrock swiveled 42 calm harps!", since=0)
    assert state.accepts("bedrock swiveled 42 calm harps!", 1)
    assert state.accepts("  Bedrock  swiveled 42 calm harps ", 1)
    assert state.accepts("bedrock swiveled 42 calm harps?", 1)
    assert not state.accepts("bedrock swiveled 43 calm harps!", 1)
    assert not state.accepts("bedrock swiveled 42 calm", 1)
    assert not state.accepts("", 1)
    assert not state.accepts("!!!", 1)


def test_the_previous_token_works_only_during_its_grace_period() -> None:
    old = session.State(current="old token 2 here.", since=0)
    new = old.rotated("new token 3 now!", now=100, grace=300)
    assert new.accepts("new token 3 now", 101)
    assert new.accepts("old token 2 here", 399)
    assert not new.accepts("old token 2 here", 401)


# -- rotation ------------------------------------------------------------------------

class Clock:
    def __init__(self, t: float = 1000.0):
        self.t = t

    def __call__(self) -> float:
        return self.t


def test_rotator_makes_a_token_rotates_on_schedule_and_on_request(tmp_path: Path) -> None:
    clock = Clock()
    state_path, shared = tmp_path / "state.json", tmp_path / "shared"
    rot = Rotator(state_path, shared, rotate_minutes=60, grace_minutes=5, clock=clock)
    first = rot.tick()
    assert (shared / "session-token").read_text() == first.current + "\n"
    assert oct((shared / "session-token").stat().st_mode & 0o777) == "0o640"
    clock.t += 59 * 60
    assert rot.tick().current == first.current
    clock.t += 61
    second = rot.tick()
    assert second.current != first.current and second.previous == first.current
    assert second.previous_until == clock.t + 300
    (shared / "rotate").touch()
    third = rot.tick()
    assert third.current != second.current
    assert not (shared / "rotate").exists()
    assert json.loads(state_path.read_text())["current"] == third.current


def test_rotator_restores_a_missing_public_copy_without_rotating(tmp_path: Path) -> None:
    rot = Rotator(tmp_path / "state.json", tmp_path / "shared", rotate_minutes=60, grace_minutes=5, clock=Clock())
    first = rot.tick()
    (tmp_path / "shared" / "session-token").unlink()
    assert rot.tick().current == first.current
    assert (tmp_path / "shared" / "session-token").read_text().strip() == first.current


def test_zero_minutes_never_rotates_by_time(tmp_path: Path) -> None:
    clock = Clock()
    rot = Rotator(tmp_path / "s.json", tmp_path / "shared", rotate_minutes=0, grace_minutes=5, clock=clock)
    first = rot.tick()
    clock.t += 10 * 24 * 3600
    assert rot.tick().current == first.current


def test_the_reader_sees_a_new_state(tmp_path: Path) -> None:
    clock = Clock()
    path = tmp_path / "state.json"
    rot = Rotator(path, tmp_path / "shared", rotate_minutes=60, grace_minutes=0, clock=clock)
    reader = session.Reader(path)
    assert not reader.accepts("anything")
    first = rot.tick()
    assert reader.accepts(first.current, clock.t)
    (tmp_path / "shared" / "rotate").touch()
    clock.t += 1
    second = rot.tick()
    assert reader.accepts(second.current, clock.t)
    assert not reader.accepts(first.current, clock.t)


# -- accounts ------------------------------------------------------------------------

def test_passwords_hash_and_check() -> None:
    stored = hash_password("correct horse")
    assert stored.startswith("scrypt$")
    assert check_password("correct horse", stored)
    assert not check_password("correct horse!", stored)
    assert not check_password("x", "garbage")


def test_accounts_create_once_and_verify(tmp_path: Path) -> None:
    accounts = Accounts(tmp_path / "a.sqlite")
    assert accounts.create("ada", "password1")
    assert not accounts.create("ada", "other-password")
    assert accounts.verify("ada", "password1")
    assert not accounts.verify("ada", "other-password")
    assert not accounts.verify("nobody", "password1")
    assert accounts.set_password("ada", "password2") and accounts.verify("ada", "password2")
    assert accounts.usernames() == ["ada"]


# -- the authenticator ------------------------------------------------------------------

jupyterhub = pytest.importorskip("jupyterhub")


@pytest.fixture
def hub(tmp_path: Path):
    from cactup_cyol.authenticator import CYOLAuthenticator

    clock = Clock()
    rot = Rotator(tmp_path / "token.json", tmp_path / "shared", rotate_minutes=60, grace_minutes=5, clock=clock)
    state = rot.tick()
    auth = CYOLAuthenticator(accounts_db=str(tmp_path / "accounts.sqlite"),
                             token_state=str(tmp_path / "token.json"), max_failures=3)
    return SimpleNamespace(auth=auth, rot=rot, clock=clock, token=state.current, tmp=tmp_path)


def login(auth, username, password, token="", ip="10.0.0.1"):
    handler = SimpleNamespace(request=SimpleNamespace(remote_ip=ip))
    return asyncio.run(auth.authenticate(handler, {"username": username, "password": password, "otp": token}))


def test_the_login_form_asks_for_the_token(hub) -> None:
    assert hub.auth.request_otp
    assert "Session token" in hub.auth.otp_prompt


def test_a_new_name_with_the_token_makes_an_account_and_logs_in(hub) -> None:
    assert login(hub.auth, "Ada", "a good password", hub.token) == "ada"
    # Back again: the password is enough, and the token field is ignored.
    assert login(hub.auth, "ada", "a good password") == "ada"
    assert login(hub.auth, "ada", "a good password", "wrong token") == "ada"
    assert login(hub.auth, "ada", "a bad password", hub.token) is None


def test_no_account_without_the_current_token(hub) -> None:
    assert login(hub.auth, "bob", "a good password") is None
    assert login(hub.auth, "bob", "a good password", "some other sentence 4 here!") is None
    assert not Accounts(hub.tmp / "accounts.sqlite").exists("bob")


def test_an_expired_token_makes_no_account_but_one_in_grace_does(hub) -> None:
    # The authenticator judges by the real clock: write states around now.
    def state(previous_until: float) -> None:
        session.write_state(hub.tmp / "token.json", session.State(
            current="new token 3 now!", since=time.time(), previous=hub.token, previous_until=previous_until))

    state(time.time() + 60)
    assert login(hub.auth, "carol", "a good password", hub.token) == "carol"
    state(time.time() - 1)
    assert login(hub.auth, "dave", "a good password", hub.token) is None
    assert login(hub.auth, "dave", "a good password", "new token 3 now") == "dave"


def test_bad_names_and_short_passwords_are_refused(hub) -> None:
    assert login(hub.auth, "9lives", "a good password", hub.token) is None
    assert login(hub.auth, "a b", "a good password", hub.token) is None
    assert login(hub.auth, "x", "a good password", hub.token) is None
    assert login(hub.auth, "eve", "short", hub.token) is None
    assert login(hub.auth, "", "a good password", hub.token) is None


def test_failures_are_limited_per_address_and_name_and_wrong_tokens_per_address(hub) -> None:
    hub.auth.max_address_failures = 6
    login(hub.auth, "frank", "a good password", hub.token)
    for _ in range(3):
        assert login(hub.auth, "frank", "wrong", ip="10.0.0.9") is None
    # Blocked now for frank from that address, even with the right password...
    assert login(hub.auth, "frank", "a good password", ip="10.0.0.9") is None
    # ...but not from another address, nor for someone else at the same one
    # (one network's attendees often share an address).
    assert login(hub.auth, "frank", "a good password", ip="10.0.0.8") == "frank"
    assert login(hub.auth, "grace", "a good password", hub.token, ip="10.0.0.9") == "grace"
    # Misspelled names with no token are typos, not token guesses: they
    # don't count against the address.
    for name in ("h1", "h2", "h3", "h4", "h5", "h6"):
        assert login(hub.auth, name, "a good password", ip="10.0.0.9") is None
    assert login(hub.auth, "henry", "a good password", hub.token, ip="10.0.0.9") == "henry"
    # Wrong tokens from one address, across names, stop signing up from it...
    for name in ("i1", "i2", "i3", "i4", "i5", "i6"):
        assert login(hub.auth, name, "a good password", "not the token", ip="10.0.0.9") is None
    assert login(hub.auth, "irene", "a good password", hub.token, ip="10.0.0.9") is None
    assert login(hub.auth, "irene", "a good password", hub.token, ip="10.0.0.7") == "irene"
    # ...but not logging in: the room behind that address gets on with it.
    assert login(hub.auth, "grace", "a good password", ip="10.0.0.9") == "grace"
    # Once the window has passed, the address may try again.
    expire(hub.auth)
    assert login(hub.auth, "frank", "a good password", ip="10.0.0.9") == "frank"
    assert login(hub.auth, "jack", "a good password", hub.token, ip="10.0.0.9") == "jack"


def expire(auth) -> None:
    for failures in auth._failures.values():
        for i, t in enumerate(failures):
            failures[i] = t - auth.failure_window - 1


def test_tries_while_blocked_dont_extend_the_block(hub) -> None:
    login(hub.auth, "ivan", "a good password", hub.token)
    for _ in range(3):
        login(hub.auth, "ivan", "wrong", ip="10.0.1.1")
    for _ in range(10):
        assert login(hub.auth, "ivan", "wrong", ip="10.0.1.1") is None
    assert len(hub.auth._failures[("pair", "10.0.1.1", "ivan")]) == 3
    expire(hub.auth)
    assert login(hub.auth, "ivan", "a good password", ip="10.0.1.1") == "ivan"


def test_one_name_is_limited_from_any_address(hub) -> None:
    # Someone who can claim any address (from inside an attendee's container,
    # the address header is whatever they send) still gets few guesses.
    hub.auth.max_user_failures = 4
    login(hub.auth, "judy", "a good password", hub.token)
    for i in range(4):
        assert login(hub.auth, "judy", "guess", ip=f"10.9.0.{i}") is None
    assert login(hub.auth, "judy", "a good password", ip="10.9.1.1") is None
    assert login(hub.auth, "kim", "a good password", hub.token, ip="10.9.1.1") == "kim"


def test_oversized_input_is_refused_unremembered(hub) -> None:
    assert login(hub.auth, "x" * 65, "a good password", hub.token) is None
    assert login(hub.auth, "leo", "p" * 1025, hub.token) is None
    assert hub.auth._failures == {}


def test_admin_names_cant_be_taken_with_the_token(hub) -> None:
    hub.auth.admin_users = {"boss"}
    assert login(hub.auth, "Boss", "a good password", hub.token) is None
    assert not Accounts(hub.tmp / "accounts.sqlite").exists("boss")
    Accounts(hub.tmp / "accounts.sqlite").create("boss", "made on the machine")
    assert login(hub.auth, "boss", "made on the machine") == "boss"


def test_the_failure_table_is_swept(hub, monkeypatch) -> None:
    from cactup_cyol import authenticator

    monkeypatch.setattr(authenticator, "SWEEP_AT", 10)
    for i in range(20):
        login(hub.auth, f"nobody{i}", "a good password", "no token", ip=f"10.8.0.{i}")
    expire(hub.auth)
    # Swept a moment ago (at the 11th key): not again so soon.
    login(hub.auth, "last", "a good password", "no token", ip="10.8.1.1")
    assert len(hub.auth._failures) > 3
    hub.auth._swept -= authenticator.SWEEP_EVERY
    login(hub.auth, "last", "a good password", "no token", ip="10.8.1.1")
    assert len(hub.auth._failures) == 3


def test_make_users_writes_accounts_that_log_in(hub, capsys, monkeypatch) -> None:
    from cactup_cyol import make_users

    monkeypatch.setattr(sys, "argv", ["make_users", "3", "--prefix", "guest", "--db", str(hub.tmp / "accounts.sqlite")])
    assert make_users.main() == 0
    lines = capsys.readouterr().out.split()
    assert [line.split(",")[0] for line in lines] == ["guest01", "guest02", "guest03"]
    name, pw = lines[0].split(",")
    assert login(hub.auth, name, pw) == name

    monkeypatch.setattr(sys, "argv", ["make_users", "--name", "Boss", "--random", "--db", str(hub.tmp / "accounts.sqlite")])
    assert make_users.main() == 0
    name, pw = capsys.readouterr().out.strip().split(",")
    assert name == "boss" and login(hub.auth, name, pw) == name

    monkeypatch.setattr(sys, "argv", ["make_users", "2", "--prefix", "Bad_Prefix", "--db", str(hub.tmp / "accounts.sqlite")])
    with pytest.raises(SystemExit):
        make_users.main()


# -- cores ----------------------------------------------------------------------------

def test_cpu_lists_and_slices() -> None:
    from cactup_cyol.cpusets import parse_cpu_list, slices

    assert parse_cpu_list("0-3,8,10-11\n") == [0, 1, 2, 3, 8, 10, 11]
    assert slices(list(range(10)), 4) == ["0,1,2,3", "4,5,6,7"]  # 8 and 9 unused
    assert slices([0, 1], 4) == ["0,1"]


def test_simultaneous_starts_get_different_slices_and_released_ones_return() -> None:
    from cactup_cyol.cpusets import Assigner

    a = Assigner(["0-3", "4-7", "8-11"])
    # Nothing exists in Docker yet: what this hub handed out counts.
    got = [a.assign(f"c{i}", running={}) for i in range(3)]
    assert sorted(got) == ["0-3", "4-7", "8-11"]
    # More attendees than slices: sharing, evenly.
    assert a.assign("c3", running={}) == "0-3"
    a.release("c1")
    assert a.assign("c4", running={}) == "4-7"
    # A restarted hub knows only what Docker says is running.
    fresh = Assigner(["0-3", "4-7", "8-11"])
    assert fresh.assign("c5", running={"c0": "0-3", "c2": "8-11"}) == "4-7"
    # A container starting again doesn't count against itself.
    assert fresh.assign("c0", running={"c0": "0-3"}) == "0-3"
