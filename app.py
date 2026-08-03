import os
import json
import time
import uuid
import random
import string
from flask import Flask, render_template, request, jsonify, send_from_directory
from werkzeug.utils import secure_filename

app = Flask(__name__)

# --- CONFIGURATION ---
BASE_DIR = os.path.dirname(os.path.abspath(__file__))
UPLOAD_FOLDER = os.path.join(BASE_DIR, 'static', 'media')
DATA_FILE = os.path.join(BASE_DIR, 'data.json')
ALLOWED_EXTENSIONS = {'png', 'jpg', 'jpeg', 'gif', 'webp', 'mp4', 'webm', 'ogg', 'mov'}

app.config['UPLOAD_FOLDER'] = UPLOAD_FOLDER
os.makedirs(UPLOAD_FOLDER, exist_ok=True)

# --- IN-MEMORY STATE STORE ---
state = {
    "displays": [],
    "media": [],
    "playlists": []
}

# --- PERSISTENCE HELPERS ---
def load_state():
    global state
    if os.path.exists(DATA_FILE):
        try:
            with open(DATA_FILE, 'r', encoding='utf-8') as f:
                loaded = json.load(f)
                state["displays"] = loaded.get("displays", [])
                state["media"] = loaded.get("media", [])
                state["playlists"] = loaded.get("playlists", [])
                print(f"[STORAGE] State loaded successfully from {DATA_FILE}")
        except Exception as e:
            print(f"[STORAGE] Error loading data.json: {e}")
            save_state()
    else:
        save_state()

def save_state():
    try:
        # Atomic write pattern to avoid file corruption
        temp_file = f"{DATA_FILE}.tmp"
        with open(temp_file, 'w', encoding='utf-8') as f:
            json.dump(state, f, indent=2)
        os.replace(temp_file, DATA_FILE)
    except Exception as e:
        print(f"[STORAGE] Error saving state to data.json: {e}")

# Load state at server launch
load_state()

# --- UTILITY FUNCTIONS ---
def allowed_file(filename):
    return '.' in filename and filename.rsplit('.', 1)[1].lower() in ALLOWED_EXTENSIONS

def get_media_type(filename_or_url):
    ext = filename_or_url.rsplit('?', 1)[0].split('.')[-1].lower()
    if ext in {'mp4', 'webm', 'ogg', 'mov'}:
        return 'video'
    elif ext in {'png', 'jpg', 'jpeg', 'gif', 'webp'}:
        return 'image'
    return 'url'

def generate_display_code():
    while True:
        code = ''.join(random.choices(string.ascii_uppercase + string.digits, k=4))
        if not any(d['code'] == code for d in state['displays']):
            return code

# --- HTML PAGE ROUTES ---
@app.route('/')
@app.route('/operator')
def operator_page():
    return render_template('operator_page.html')

@app.route('/display')
def display_page():
    return render_template('display_page.html')

@app.route('/static/media/<filename>')
def serve_upload(filename):
    return send_from_directory(app.config['UPLOAD_FOLDER'], filename)

# --- API ENDPOINTS ---

@app.route('/api/state', methods=['GET'])
def get_state():
    now = time.time()
    formatted_displays = []

    for d in state['displays']:
        is_online = (now - d.get('last_seen', 0)) < 5
        formatted_displays.append({
            "code": d['code'],
            "linked": d.get('linked', False),
            "active_playlist_id": d.get('active_playlist_id'),
            "online": is_online,
            "status": "online" if is_online else "offline"
        })

    return jsonify({
        "displays": formatted_displays,
        "media": state['media'],
        "playlists": state['playlists']
    })

@app.route('/api/display/register', methods=['POST'])
def register_display():
    data = request.json or {}
    requested_code = data.get('code', '').strip().upper()

    # Re-register an existing known code if provided
    if requested_code and len(requested_code) == 4:
        display = next((d for d in state['displays'] if d['code'] == requested_code), None)
        if display:
            display['last_seen'] = time.time()
            return jsonify({"success": True, "code": requested_code, "reconnected": True})

    code = generate_display_code()
    new_display = {
        "code": code,
        "linked": False,
        "active_playlist_id": None,
        "last_seen": time.time()
    }
    state['displays'].append(new_display)
    save_state()
    return jsonify({"success": True, "code": code, "reconnected": False})

@app.route('/api/display/<code_id>', methods=['GET'])
def poll_display(code_id):
    code_id = code_id.upper()
    display = next((d for d in state['displays'] if d['code'] == code_id), None)

    if not display:
        return jsonify({"error": "Display not found"}), 404

    # Update heartbeat timestamp
    display['last_seen'] = time.time()

    if not display.get('linked', False):
        return jsonify({"linked": False, "code": code_id})

    # Resolve active playlist content
    active_playlist = next((p for p in state['playlists'] if p['id'] == display.get('active_playlist_id')), None)
    
    enriched_media = []
    if active_playlist and 'items' in active_playlist:
        for item in active_playlist['items']:
            media_obj = next((m for m in state['media'] if m['id'] == item['media_id']), None)
            if media_obj:
                enriched_media.append({
                    "id": media_obj['id'],
                    "name": media_obj['name'],
                    "url": media_obj['url'],
                    "type": media_obj['type'],
                    "duration": item.get('duration', 10)
                })

    return jsonify({
        "linked": True,
        "code": code_id,
        "active_playlist_id": display.get('active_playlist_id'),
        "media": enriched_media
    })

@app.route('/api/link_display', methods=['POST'])
def link_display():
    data = request.json or {}
    code = data.get('code', '').upper()
    display = next((d for d in state['displays'] if d['code'] == code), None)

    if not display:
        return jsonify({"success": False, "message": "Invalid display code."}), 404

    display['linked'] = True
    save_state()
    return jsonify({"success": True, "display": display})

@app.route('/api/unlink_display', methods=['POST'])
def unlink_display():
    data = request.json or {}
    code = data.get('code', '').upper()
    display = next((d for d in state['displays'] if d['code'] == code), None)

    if display:
        display['linked'] = False
        display['active_playlist_id'] = None
        save_state()

    return jsonify({"success": True})

@app.route('/api/activate_playlist', methods=['POST'])
def activate_playlist():
    data = request.json or {}
    code = data.get('code', '').upper()
    playlist_id = data.get('playlist_id')

    display = next((d for d in state['displays'] if d['code'] == code), None)
    if display:
        display['active_playlist_id'] = playlist_id if playlist_id else None
        save_state()
        return jsonify({"success": True})

    return jsonify({"success": False, "message": "Display not found"}), 404

@app.route('/api/save_playlist', methods=['POST'])
def save_playlist():
    data = request.json or {}
    playlist_id = data.get('id')
    name = data.get('name', 'Untitled Playlist')
    items = data.get('items', [])

    if playlist_id:
        pl = next((p for p in state['playlists'] if p['id'] == playlist_id), None)
        if pl:
            pl['name'] = name
            pl['items'] = items
            save_state()
            return jsonify({"success": True, "playlist": pl})

    new_pl = {
        "id": f"p_{uuid.uuid4().hex[:8]}",
        "name": name,
        "items": items
    }
    state['playlists'].append(new_pl)
    save_state()
    return jsonify({"success": True, "playlist": new_pl})

@app.route('/api/delete_playlist', methods=['POST'])
def delete_playlist():
    data = request.json or {}
    playlist_id = data.get('id')

    state['playlists'] = [p for p in state['playlists'] if p['id'] != playlist_id]

    for d in state['displays']:
        if d.get('active_playlist_id') == playlist_id:
            d['active_playlist_id'] = None

    save_state()
    return jsonify({"success": True})

@app.route('/api/upload_media', methods=['POST'])
def upload_media():
    name = request.form.get('name', 'Untitled')
    url = request.form.get('url', '')

    # Handle Direct File Upload
    if 'file' in request.files:
        file = request.files['file']
        if file and allowed_file(file.filename):
            filename = secure_filename(file.filename)
            unique_name = f"{uuid.uuid4().hex[:6]}_{filename}"
            file_path = os.path.join(app.config['UPLOAD_FOLDER'], unique_name)
            file.save(file_path)

            file_url = f"/static/media/{unique_name}"
            media_type = get_media_type(filename)

            new_media = {
                "id": f"m_{uuid.uuid4().hex[:8]}",
                "name": name if name and name != 'Untitled' else filename,
                "url": file_url,
                "type": media_type
            }
            state['media'].append(new_media)
            save_state()
            return jsonify({"success": True, "media": new_media})

    # Handle External URL
    if url:
        media_type = get_media_type(url)
        new_media = {
            "id": f"m_{uuid.uuid4().hex[:8]}",
            "name": name if name else "URL Resource",
            "url": url,
            "type": media_type
        }
        state['media'].append(new_media)
        save_state()
        return jsonify({"success": True, "media": new_media})

    return jsonify({"success": False, "message": "No file or valid URL provided."}), 400

@app.route('/api/delete_media', methods=['POST'])
def delete_media():
    data = request.json or {}
    media_id = data.get('id')

    state['media'] = [m for m in state['media'] if m['id'] != media_id]

    for p in state['playlists']:
        p['items'] = [item for item in p.get('items', []) if item['media_id'] != media_id]

    save_state()
    return jsonify({"success": True})

if __name__ == '__main__':
    print("\n-------------------------------------------------------------")
    print(" Digital Signage Server Started!")
    print(" - Operator Console: http://127.0.0.1:5000/operator")
    print(" - Display Player:    http://127.0.0.1:5000/display")
    print(" - Storage File:     data.json")
    print(" - Uploads Path:     static/media/")
    print("-------------------------------------------------------------\n")
    app.run(debug=True, port=5000)