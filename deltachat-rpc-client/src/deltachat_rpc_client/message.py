from typing import TYPE_CHECKING

from .contact import Contact
from .rpc import Rpc
from .utils import AttrDict

if TYPE_CHECKING:
    from .account import Account


class Message:
    """Delta Chat Message object."""

    def __init__(self, account: "Account", msg_id: int) -> None:
        self.account = account
        self.id = msg_id

    def __eq__(self, other) -> bool:
        if not isinstance(other, Message):
            return False
        return self.id == other.id and self.account == other.account

    def __ne__(self, other) -> bool:
        return not self == other

    def __repr__(self) -> str:
        return f"<Message id={self.id} account={self.account.id}>"

    @property
    def _rpc(self) -> Rpc:
        return self.account._rpc

    async def send_reaction(self, reactions: str) -> "Message":
        msg_id = await self._rpc.send_reaction(self.account.id, self.id, reactions)
        return Message(self.account, msg_id)

    async def get_snapshot(self) -> AttrDict:
        """Get a snapshot with the properties of this message."""
        from .chat import Chat

        snapshot = AttrDict(await self._rpc.get_message(self.account.id, self.id))
        snapshot["chat"] = Chat(self.account, snapshot.chat_id)
        snapshot["sender"] = Contact(self.account, snapshot.from_id)
        snapshot["message"] = self
        return snapshot

    async def mark_seen(self) -> None:
        """Mark the message as seen."""
        await self._rpc.markseen_msgs(self.account.id, [self.id])
