from ephemeral_port_reserve import reserve
from pathlib import Path
from pyln.testing.btcproxy import BitcoinRpcProxy
from pyln.testing.utils import SimpleBitcoinProxy
from typing import List


class ExternalBitcoinD:
    """Stand-in for pyln's `BitcoinD` that attaches to an already
    running bitcoind (e.g., a custom signet) instead of spawning a
    regtest one.
    """

    def __init__(
        self,
        directory: Path,
        host: str,
        rpcport: int,
        rpcuser: str,
        rpcpassword: str,
        chain: str = "signet",
    ):
        self.rpcport = rpcport
        self.chain = chain
        self.proxies: List[BitcoinRpcProxy] = []

        # Dedicated conf file, read by bitcoinlib's RawProxy, which
        # ignores sections, so we don't reuse the node's bitcoin.conf.
        directory.mkdir(parents=True, exist_ok=True)
        self.conf_file = str(directory / "bitcoin-rpc.conf")
        with open(self.conf_file, "w") as f:
            f.write(
                f"rpcconnect={host}\n"
                f"rpcport={rpcport}\n"
                f"rpcuser={rpcuser}\n"
                f"rpcpassword={rpcpassword}\n"
            )

        self.rpc = SimpleBitcoinProxy(btc_conf_file=self.conf_file)

    def start(self):
        info = self.rpc.getblockchaininfo()
        if info["chain"] != self.chain:
            raise ValueError(
                f"bitcoind is on chain={info['chain']}, expected {self.chain}"
            )
        return info

    def get_proxy(self) -> BitcoinRpcProxy:
        proxy = BitcoinRpcProxy(self, rpcport=reserve())
        self.proxies.append(proxy)
        proxy.start()
        return proxy

    def stop(self):
        # Only tear down our proxies, the bitcoind isn't ours to stop.
        for p in self.proxies:
            p.stop()
