# Privacy guide

homeguard records images of people, knows when each household member is at
home, and senses movement through walls with wisense. Used carelessly, that
is surveillance of the people you live with and of your neighbours. This page
explains how to use it fairly and within the law.

This is practical guidance, not legal advice. When in doubt, ask a lawyer or
the data protection authority.

## 1. Only film your own property

- Point cameras at **your own** doors, garden, driveway and rooms.
- **Do not film the street, the pavement, or your neighbours'** windows,
  gardens or doors. If a camera's view unavoidably includes part of a public
  or neighbouring space, hide that part with a privacy mask, a black area
  Frigate removes from recordings and detection (Frigate UI → the camera →
  *Masks / Zones*, or `motion.mask` / `objects.mask` in the config).
- Avoid filming shared areas of a building (stairwells, shared courtyards,
  parking) unless the co-owners agree, and check the rules that apply to
  them.
- Indoor cameras: think twice before putting them in bedrooms or bathrooms.
  Use the `home` arm mode, in which cameras are not alarm sources by default,
  and consider turning indoor cameras off (Frigate's camera *enabled*
  switch) when people are at home.

## 2. Tell everyone

Everyone who lives in or regularly visits the home has a right to know what
is monitored:

- **Household members**: show them where the cameras are, what wisense does
  (it senses movement through walls from WiFi signals, without images), that
  their phone's presence on the WiFi is tracked, and who can see the
  dashboard, recordings and alerts.
- **Guests, babysitters, cleaners, workers**: tell them before they come in,
  or put a visible sign at the entrance ("This home is protected by video
  surveillance"). Recording someone who does not know is unfair and may be
  unlawful.
- **Domestic employees** have specific rights as workers. Tell them in
  writing what is recorded and why, never film their private moments (rest
  area, changing), and check the obligations that apply to employers.
- **Presence detection**: only add phones whose owners agree. A person can
  opt out by keeping MAC randomization on; homeguard will then not recognize
  their phone.

## 3. The law in Morocco: Law 09-08 and the CNDP

In Morocco, personal data, including images of identifiable people, is
protected by **Law No. 09-08** on the protection of individuals with regard
to the processing of personal data, overseen by the **CNDP** (Commission
Nationale de contrôle de la protection des Données à caractère Personnel,
[www.cndp.ma](https://www.cndp.ma/)).

- Filming **only inside your own private space** for your household's
  security is generally treated as a personal or household use.
- As soon as the cameras capture **people outside that private space** (the
  street, neighbours, shared areas of a building), or film **employees**,
  the recording can fall under Law 09-08. That can bring obligations such as
  informing the people filmed, limiting retention, securing the recordings,
  and declaring the system to the CNDP or getting its authorization.
- The CNDP publishes guidance and forms on video surveillance. **Check its
  current rules before filming anything beyond your own private space**, and
  ask it directly if your situation is unclear.

If you live elsewhere, similar rules apply in most countries (for example
the GDPR in the European Union); check your local data protection authority.

## 4. Keep recordings only as long as needed

- Frigate here keeps **event recordings for 7 days** and snapshots for 7 days
  (`record.alerts.retain.days`, `record.detections.retain.days` and
  `snapshots.retain.default` in `config/frigate/config.yml`). Continuous
  recording is off. Keep it that way unless you have a reason.
- homeguard's event log keeps **90 days** (`[log] retention` in
  `config/homeguard.toml`). It holds no images, but it does show when people
  came and went. Shorten it if you do not need that history.
- When an incident happens, export the relevant clip from Frigate (it then
  survives the retention period) and hand it to the police if needed; do not
  keep everything "just in case".
- Do not share recordings of identifiable people on social media.

## 5. Keep the data at home and secure

homeguard keeps everything on the hub: no images go to any cloud, except the
snapshot attached to a Telegram alert (it is then stored by Telegram in the
chats that received it). If that matters to you, keep `TELEGRAM_CHAT_IDS`
to the people who really need alerts, and delete old alert messages from
those chats now and then.

Protect the hub and the recordings as described in
[hardening.md](hardening.md): strong passwords, no port forwarding, cameras
blocked from the internet, backups encrypted.

## 6. Checklist

- [ ] Cameras show only my property; privacy masks over any street or neighbour
- [ ] Every household member knows about the cameras, wisense and presence
- [ ] A sign or a word to visitors; employees informed in writing
- [ ] Phones in presence detection belong to people who agreed
- [ ] Retention set as short as I need (Frigate 7 days, event log 90 days)
- [ ] Checked the CNDP's rules if anything beyond my private space is filmed
