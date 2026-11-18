# DIY Signage Software Setup Instructions
This project implements a multi-component signage system using Flask for the backend and Jinja2 templates for frontend rendering.

## Core Technologies
*   **Backend:** Python 3, Flask (v2.3.3)
*   **Frontend:** HTML, CSS, JavaScript (Vanilla JS/jQuery would be ideal for AJAX updates)
*   **Architecture:** Client-Server via dedicated routes (`/display` and `/operator`).

## Project Structure Overview
The following files are key to the project's function:
*   `app.py`: The main Flask application logic, handling routing, state management (mocked), and file upload processing.
*   `templates/operator_page.html`: The administrative interface for operators to manage displays and upload media.
*   `templates/display_page.html`: The public-facing page displayed on the physical signage client unit.
*   `requirements.txt`: Lists necessary Python dependencies (`Flask`, `flask-cors`).

## Implementation Details & Tasks Remaining (To be completed by developer):
1.  **Database Integration:** Replace mock dictionaries (`CONNECTED_DISPLAYS`, `UPLOADED_MEDIA`) in `app.py` with a persistent database layer (e.g., SQLAlchemy/SQLite) to ensure state persists across server restarts.
2.  **Media Storage:** Implement actual file saving logic for images and videos inside the `/static/media` folder, ensuring correct path handling when generating URLs. The current implementation only mocks this.
3.  **Client Communication (Real-time):** Replace simple HTTP requests with a real-time mechanism (e.g., WebSockets using Flask-SocketIO) to allow the server to push updates or status changes immediately to clients without requiring manual refresh.
4.  **Webpage Embedding:** When embedding external web content, ensure robust error handling and consider implementing dynamic iframe content loading based on user settings.

## Usage Instructions
1.  **Prerequisites:** Ensure Python 3 is installed.
2.  **Setup:** Run `pip install -r requirements.txt` in the project root directory (`/home/woutvde/Documents/Git_repos/fossignage`).
3.  **Run Server:** Execute the application using `python app.py`. The server will typically run on `http://127.0.0.1:5000`.
4.  **Access Pages:**
    *   **Operator Control:** Access at `http://127.0.0.1:5000/operator` (Use this page to manage displays and media).
    *   **Display Client:** The display screen should be accessed by a client device pointed to `http://127.0.0.1:5000/display`.

## Features Checklist Status
- [x] **Server Structure:** Flask project set up in `/home/woutvde/Documents/Git_repos/fossignage`.
- [x] **Dependencies:** `requirements.txt` created with core flask libraries.
- [x] **Operator Page (`operator_page.html`):** Basic layout, display registration form, media upload UI, and embed web page section implemented.
- [x] **Client Page (`display_page.html`):** Implemented to show a generated 4-character alphanumeric code on load.
- [ ] **Persistence:** Mocked data stores need replacement with actual database logic.
- [ ] **Real-time Updates:** WebSockets are required for production use (suggesting Flask-SocketIO).