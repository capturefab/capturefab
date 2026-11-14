#!/usr/bin/env python3
"""Verify captured camera XML against Capturefab's GenApi parser.

Downloads pinned, public camera descriptions to a temporary directory; no vendor
XML is redistributed with Capturefab. The areaDetector project captured these
files from devices. Two exports have a diagnostic line before their XML document;
that line is removed before checking the document hash. This checks parser/link
compatibility, not camera I/O or physical-device qualification.

Run `cargo fetch --locked` first. CARGO_TARGET_DIR is honored; tests do not replace
the capturefab CLI executable. Python's standard library is the only dependency.
"""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
COMMIT = "6d6171d0a82d669ca1fb1ebcb9b3fd5b8b055125"
BASE = f"https://raw.githubusercontent.com/areaDetector/aravisGigE/{COMMIT}/etc/genicam/"
MAX_XML = 32 * 1024 * 1024
# SHA-256 of each complete XML document, without an export's diagnostic preamble.
DOCUMENTS = {
    'AVT_Manta_G125B.xml': '0870a2d9a7d8ed4bd31d39c244d13b1b38b801527d0b6d047e978a0ac763ad2f',
    'AVT_Manta_G125B_1.44.0.xml': '137702788a4f6da3bd703d45bf87c62970e537cf19b0c06f0391e8fb960417bb',
    'AVT_Manta_G125B_1.44.4.xml': '7267cb53a081b1e9d74f9d08f9bbc8416da6e0dc46240357c416aeaae59d20e4',
    'AVT_Manta_G146B.xml': '0870a2d9a7d8ed4bd31d39c244d13b1b38b801527d0b6d047e978a0ac763ad2f',
    'Basler_acA1600_20gm.xml': '86fa55a25d4c7380a9fe719278ea54d2661da099ec35d0deb5ec4e610be7ba78',
    'Basler_piA640_210gm.xml': '822ae259fa6e74732daa3dcc673724cdfd7119f76eb5c3f646fcf661165742fe',
    'Baumer_HXG20.xml': '3f525272e13954925997abd248e329846dfa58dea990ae6e2c4cadd0a8aacb50',
    'Baumer_TXG08c.xml': '7295328050d9885c9560bc9a98bfd094eab2c1accbb0a76b25b02040768ca03e',
    'Baumer_TXG14c.xml': '467fe5c151383db134e0753a725e7d4cea18baa558ebfc86ab65b15139d3e23f',
    'FLIR_ORX_10G_51S5M.xml': '697e816477328e5a1fa2d9859bd2d905818318b49f286e68a2852b3e0893aa19',
    'JAI_RM-6740GE.xml': '219375a990fa3d3bb4867dd852d9c31c1f96a90d1d7e7a2a7de27dbca2cb5dc6',
    'PCO1600.xml': '6dbe16c7783f08b222f003364e89ba65cf753fa57b8305a07fe8b391e982327f',
    'PGR_BlackflyS_13Y3M.xml': '7f8d26e3905801935410920450c40e8f97da1dc66a55d45d3d1841f07b0f1083',
    'PGR_Blackfly_13E4C-CS.xml': '7904cf214461356facdf345ee1551f784ad6f100e2013741774a807b78a56780',
    'PGR_Blackfly_20E4C.xml': 'e0f39f1ebd024511bb40339ffd25ed4bd044b3cfa20f9de2d730d889215d1e85',
    'PGR_Flea3.xml': 'ec3e7b9bc666523e8baaea84033eb68ea5262f23be56ec7b65500867b04c3ec8',
    'PGR_GS3_U3_23S6M.xml': '6f7cb1fb046bb5f02290e3f0c63c0424eb7973baf93d3242ec9596c862df6eb0',
    'Photonic_Science_FDI3.xml': 'fa5ba5a1665f94afcb5616568ca2eaccf6ed0f2ba279f250cd2dccd170eb3e76',
    'Photonic_Science_LC_SCMOS.xml': '6da5d463d53828b05a93637a92b4d593bf4924a88d54bb199a774bfde44174de',
    'Prosilica_GC1020.xml': '6bca86621546333090f98bb2344d9a0f122d218ec544d9c653e8028a941fb466',
    'Prosilica_GC1020C.xml': '6bca86621546333090f98bb2344d9a0f122d218ec544d9c653e8028a941fb466',
    'Prosilica_GC1280M.xml': '8866af409dbc3e6bd812cdb81850a0cb7f1bab3eca31a1d4aff77b33582eb998',
    'SVS_evo4050.xml': 'd9051235e3cd3ca1c332c6d4bcf5d04ed5973db9f80dd84f390f5d1280aea0af',
    'Sony_CVC_EH6300.xml': 'ac7ea9ad5626681478aa1e0d1c7ee47b34dd48d486c708eb3a26dfb0dbe87445',
    'TIS_dmk_33GX174.xml': 'c8e4f76863281ff30812c66dac307f105c8b4fad7bb3e9544194b735b83ff7a7',
    'TXG13_new.xml': '8339f4b3cb715e7c9df06d2aeb2d11877491e9a44578128173707cc308eb4a4f',
    'TXG13_old.xml': 'd1a484d3ed5c7f63b1cab01a3fa5c8f62d83888262344f9aa990f3250a462582',
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cargo', default='cargo', help='Cargo executable')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='capturefab-camera-xml-') as temporary:
        directory = Path(temporary)
        for name, digest in DOCUMENTS.items():
            with urllib.request.urlopen(BASE + name, timeout=30) as response:
                payload = response.read(MAX_XML + 1)
            if len(payload) > MAX_XML:
                parser.error(f'{name}: source exceeds 32 MiB')
            starts = [offset for marker in (b'<?xml', b'<RegisterDescription')
                      if (offset := payload.find(marker)) >= 0]
            if not starts:
                parser.error(f'{name}: XML root is missing')
            document = payload[min(starts):]
            if hashlib.sha256(document).hexdigest() != digest:
                parser.error(f'{name}: pinned XML hash mismatch')
            (directory / name).write_bytes(document)
        environment = os.environ.copy()
        environment['CAPTUREFAB_VENDOR_XML_DIR'] = str(directory)
        environment['CAPTUREFAB_VENDOR_XML_COUNT'] = str(len(DOCUMENTS))
        command = [args.cargo, 'test', '--offline', '--locked', '--no-default-features',
                   '--lib', 'genicam::external_vendor_audit::',
                   '--', '--ignored', '--nocapture']
        result = subprocess.run(command, cwd=ROOT, env=environment, check=False).returncode
        if result == 0:
            print(f'Verified parser, links and capture nodes in {len(DOCUMENTS)} pinned camera descriptions.')
        return result


if __name__ == '__main__':
    raise SystemExit(main())
