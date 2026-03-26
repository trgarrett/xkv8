import json

def _hexify(obj):
    if obj is None:
        return None
    for method_name in ("to_bytes", "serialize", "to_bytes_hex"):
        method = getattr(obj, method_name, None)
        if callable(method):
            try:
                b = method()
                if isinstance(b, str):
                    return b
                return b.hex()
            except Exception:
                continue
    try:
        return bytes(obj).hex()
    except Exception:
        try:
            return str(obj)
        except Exception:
            return None


def spend_bundle_to_json(bundle):
    def _0x(val):
        h = _hexify(val)
        if h is not None and not h.startswith("0x"):
            return "0x" + h
        return h

    data = {
        "aggregated_signature": _0x(getattr(bundle, "aggregated_signature", None)),
        "coin_spends": [],
    }

    for cs in getattr(bundle, "coin_spends", []):
        coin = getattr(cs, "coin", None)
        puzzle_reveal = _0x(getattr(cs, "puzzle_reveal", None))
        solution = _0x(getattr(cs, "solution", None))

        data["coin_spends"].append({
            "coin": {
                "parent_coin_info": _0x(getattr(coin, "parent_coin_info", None)),
                "puzzle_hash": _0x(getattr(coin, "puzzle_hash", None)),
                "amount": getattr(coin, "amount", None),
            },
            "puzzle_reveal": puzzle_reveal,
            "solution": solution,
        })

    return json.dumps(data, indent=4)