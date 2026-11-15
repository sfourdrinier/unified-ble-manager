"""Exercise reconstructed pre-V2 and current generated Python request codecs.

Arguments are directories containing ubm_echo.py and the matching libuniffi.
This checks enum encoding, not negotiation with an older native library.
"""
import importlib.util
import struct
import sys
from pathlib import Path


def load(name, directory):
    spec = importlib.util.spec_from_file_location(name, Path(directory) / "ubm_echo.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


class Writer:
    def __init__(self):
        self.data = bytearray()

    def write_i32(self, value):
        self.data.extend(struct.pack(">i", value))

    def write_u32(self, value):
        self.data.extend(struct.pack(">I", value))

    def write_u64(self, value):
        self.data.extend(struct.pack(">Q", value))


def encode(module, value):
    writer = Writer()
    module._UniffiFfiConverterTypeMobileRadioRequest.write(value, writer)
    return bytes(writer.data)


def decode(module, data):
    stream = module._UniffiRustBufferStream(data, len(data))
    result = module._UniffiFfiConverterTypeMobileRadioRequest.read(stream)
    assert stream.remaining() == 0
    return result


old, new = load("old_ubm_echo", sys.argv[1]), load("new_ubm_echo", sys.argv[2])
for producer, consumer in [(old, new), (new, old)]:
    generation = producer.MobileOrderedGeneration.COUNTER(17)
    for name, args in [("ORDERED_BARRIER", (7, generation, 23, 42)),
                       ("ORDERED_ACKNOWLEDGE_BATCH", (8, generation, 23, 44))]:
        value = getattr(producer.MobileRadioRequest, name)(*args)
        decoded = decode(consumer, encode(producer, value))
        assert getattr(decoded, "is_" + name)()
        assert decoded.id == args[0] and decoded.group_id == 23
        assert decoded.generation.value == 17
        assert (decoded.barrier_id if name == "ORDERED_BARRIER" else decoded.batch_id) == args[3]
for value in [new.MobileRadioRequest.ORDERED_DRAIN_V2(9, 2, new.MobileOrderedGeneration.COUNTER(17), 23),
              new.MobileRadioRequest.ORDERED_ACKNOWLEDGE_BATCH_V2(10, 2, new.MobileOrderedGeneration.COUNTER(17), 23, 44)]:
    wire = encode(new, value)
    assert type(decode(new, wire)) is type(value)
    try:
        decode(old, wire)
    except old.InternalError:
        pass
    else:
        raise AssertionError("old binding decoded a new request")
print("Generated old/new Python request codecs preserve legacy variants and reject V2 in the old decoder")
