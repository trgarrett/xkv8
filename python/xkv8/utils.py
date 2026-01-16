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
    data = {
        "aggregated_signature": _hexify(getattr(bundle, "aggregated_signature", None)),
        "coin_spends": [],
    }

    for cs in getattr(bundle, "coin_spends", []):
        coin = getattr(cs, "coin", None)
        puzzle_reveal = _hexify(getattr(cs, "puzzle_reveal", None))
        solution = _hexify(getattr(cs, "solution", None))

        data["coin_spends"].append({
            "coin": {
                "parent_coin_info": _hexify(getattr(coin, "parent_coin_info", None)),
                "puzzle_hash": _hexify(getattr(coin, "puzzle_hash", None)),
                "amount": getattr(coin, "amount", None),
            },
            "puzzle_reveal": puzzle_reveal,
            "solution": solution,
        })

    return json.dumps(data, indent=4)
