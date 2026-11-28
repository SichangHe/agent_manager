"""Mandarin front end for Kokoro-82M v1.0 (lang_code "z"): fixes polyphones, tone sandhi and clause pauses.

misaki 0.9.4's default zh G2P (`ZHG2P(version=None)`) is jieba segmentation + pypinyin's small default phrase dictionary
and no tone sandhi, and Kokoro cuts text only at ASCII `.!?`, so a whole Chinese paragraph is one utterance with
the same short pause at every punctuation mark. Here: jieba POS segmentation, pypinyin with the large phrase dictionary,
misaki's 3-3, 一 and 不 tone sandhi, a few extra phrase fixes, then misaki's own pinyin-to-IPA so the v1.0 voices still get the phonemes they know.
"""

import re

import cn2an
import jieba
import jieba.posseg as psg
from misaki.tone_sandhi import ToneSandhi
from misaki.zh import ZHG2P
from pypinyin import Style, lazy_pinyin, load_phrases_dict, load_single_dict
from pypinyin_dict.phrase_pinyin_data import large_pinyin

# Words jieba splits or pypinyin misreads; each is forced to be one word with the given reading.
# 还 is huan2 ("give back") before an object or particle, hai2 ("still") otherwise (the default).
EXTRA_PHRASES = {
    "还了": ["huan2", "le5"], "还钱": ["huan2", "qian2"], "还书": ["huan2", "shu1"], "还债": ["huan2", "zhai4"],
    "还款": ["huan2", "kuan3"], "还给": ["huan2", "gei3"], "还清": ["huan2", "qing1"], "还回": ["huan2", "hui2"],
    "还掉": ["huan2", "diao4"], "还完": ["huan2", "wan2"], "还的": ["huan2", "de5"], "归还": ["gui1", "huan2"],
    "偿还": ["chang2", "huan2"], "退还": ["tui4", "huan2"], "返还": ["fan3", "huan2"], "还贷": ["huan2", "dai4"],
    "相当": ["xiang1", "dang1"], "行人": ["xing2", "ren2"], "处理": ["chu3", "li3"],
}
_loaded = False


def load() -> None:
    global _loaded
    if _loaded:
        return
    large_pinyin.load()
    load_phrases_dict({w: [[p] for p in py] for w, py in EXTRA_PHRASES.items()})
    load_single_dict({ord("地"): "de,di4"})
    for w in EXTRA_PHRASES:
        jieba.add_word(w, freq=10**6)
    _loaded = True


_sandhi = ToneSandhi()
HAN = re.compile("[一-鿿]+|[^一-鿿]+")


def pinyin_words(text: str) -> list[list[str]]:
    """Per jieba word, tone-numbered pinyin (neutral tone 5) of each Han character, after tone sandhi."""
    load()
    out = []
    for word, pos in _sandhi.pre_merge_for_modify(psg.lcut(text)):
        py = lazy_pinyin(word, style=Style.TONE3, neutral_tone_with_five=True)
        finals = lazy_pinyin(word, style=Style.FINALS_TONE3, neutral_tone_with_five=True)
        # Not ToneSandhi's neutral-tone rules: they turn the 2nd of 行行 and the 现 of 发现 neutral.
        new = _sandhi._three_sandhi(word, _sandhi._yi_sandhi(word, _sandhi._bu_sandhi(word, list(finals))))
        out.append([re.sub(r"\d$", n[-1], p) if p[-1].isdigit() and n[-1].isdigit() else p for p, n in zip(py, new)])
    return out


def pinyin(text: str) -> list[str]:
    """Flat per-character form of `pinyin_words`."""
    return [p for w in pinyin_words(text) for p in w]


def g2p(text: str, *args, **kwargs) -> tuple[str, None]:
    """Drop-in for `KPipeline.g2p` of lang_code "z": text to Kokoro v1.0 phonemes."""
    if not text.strip():
        return "", None
    text = ZHG2P.map_punctuation(cn2an.transform(text, "an2cn"))
    ps = ""
    for seg in HAN.findall(text):
        if "一" <= seg[0] <= "鿿":
            # Kokoro hears word boundaries as spaces (as in misaki's own zh G2P), never inside a word
            ps += " ".join("".join(ZHG2P.py2ipa(p) for p in w) for w in pinyin_words(seg))
        else:
            ps += seg
    return ps.replace(chr(815), ""), None


# Pause (ms) after a clause, on top of the ~70 ms Kokoro's trimmed edges leave.
PAUSE_MS = {"。": 200, "！": 200, "？": 200, "；": 150, "：": 130, "，": 70, "、": 40}
CLAUSE = re.compile(r"[^。！？；：，、]+[。！？；：，、]*|[。！？；：，、]+")
MIN_CLAUSE = 4  # shorter clauses (e.g. "好，") are joined to the next one: a few syllables alone sound clipped


def clauses(text: str) -> list[tuple[str, int]]:
    """Cut Chinese text after each of 。！？；：，、 into `(clause incl. its punctuation, pause ms after)`."""
    out: list[tuple[str, int]] = []
    carry = ""
    for m in CLAUSE.findall(text):
        piece = carry + m
        marks = re.search(r"[。！？；：，、]+$", piece)
        pause = max((PAUSE_MS[c] for c in marks.group()), default=0) if marks else 0
        if len(re.findall(r"[㐀-鿿]", piece)) < MIN_CLAUSE and pause < PAUSE_MS["。"]:
            carry = piece
            continue
        carry = ""
        out.append((piece, pause))
    if carry:
        if out:
            out[-1] = (out[-1][0] + carry, out[-1][1])
        else:
            out.append((carry, 0))
    return out
