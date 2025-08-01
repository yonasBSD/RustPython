from test import support
from test.test_json import PyTest, CTest

import unittest # XXX: RUSTPYTHON; importing to be able to skip tests


class JSONTestObject:
    pass


class TestRecursion:
    def test_listrecursion(self):
        x = []
        x.append(x)
        try:
            self.dumps(x)
        except ValueError:
            pass
        else:
            self.fail("didn't raise ValueError on list recursion")
        x = []
        y = [x]
        x.append(y)
        try:
            self.dumps(x)
        except ValueError:
            pass
        else:
            self.fail("didn't raise ValueError on alternating list recursion")
        y = []
        x = [y, y]
        # ensure that the marker is cleared
        self.dumps(x)

    def test_dictrecursion(self):
        x = {}
        x["test"] = x
        try:
            self.dumps(x)
        except ValueError:
            pass
        else:
            self.fail("didn't raise ValueError on dict recursion")
        x = {}
        y = {"a": x, "b": x}
        # ensure that the marker is cleared
        self.dumps(x)

    def test_defaultrecursion(self):
        class RecursiveJSONEncoder(self.json.JSONEncoder):
            recurse = False
            def default(self, o):
                if o is JSONTestObject:
                    if self.recurse:
                        return [JSONTestObject]
                    else:
                        return 'JSONTestObject'
                return self.json.JSONEncoder.default(o)

        enc = RecursiveJSONEncoder()
        self.assertEqual(enc.encode(JSONTestObject), '"JSONTestObject"')
        enc.recurse = True
        try:
            enc.encode(JSONTestObject)
        except ValueError:
            pass
        else:
            self.fail("didn't raise ValueError on default recursion")

    # TODO: RUSTPYTHON
    @unittest.skip("TODO: RUSTPYTHON; crashes")
    def test_highly_nested_objects_decoding(self):
        # test that loading highly-nested objects doesn't segfault when C
        # accelerations are used. See #12017
        with self.assertRaises(RecursionError):
            with support.infinite_recursion():
                self.loads('{"a":' * 100000 + '1' + '}' * 100000)
        with self.assertRaises(RecursionError):
            with support.infinite_recursion():
                self.loads('{"a":' * 100000 + '[1]' + '}' * 100000)
        with self.assertRaises(RecursionError):
            with support.infinite_recursion():
                self.loads('[' * 100000 + '1' + ']' * 100000)

    @support.requires_resource('cpu')
    def test_highly_nested_objects_encoding(self):
        # See #12051
        l, d = [], {}
        for x in range(100000):
            l, d = [l], {'k':d}
        with self.assertRaises(RecursionError):
            with support.infinite_recursion(5000):
                self.dumps(l)
        with self.assertRaises(RecursionError):
            with support.infinite_recursion(5000):
                self.dumps(d)

    def test_endless_recursion(self):
        # See #12051
        class EndlessJSONEncoder(self.json.JSONEncoder):
            def default(self, o):
                """If check_circular is False, this will keep adding another list."""
                return [o]

        with self.assertRaises(RecursionError):
            with support.infinite_recursion(1000):
                EndlessJSONEncoder(check_circular=False).encode(5j)


class TestPyRecursion(TestRecursion, PyTest): pass
class TestCRecursion(TestRecursion, CTest): pass
