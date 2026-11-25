// What a site does after `npm install @liberika/web`: a bare import, bundled.
import { Player } from "@liberika/web";

window.playerReady = Player.create({
  canvas: document.querySelector("#video"),
}).then((player) => (window.player = player));
